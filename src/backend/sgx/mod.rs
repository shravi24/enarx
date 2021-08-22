// SPDX-License-Identifier: Apache-2.0

use crate::backend::sgx::attestation::get_attestation;
use crate::backend::{Command, Datum, Keep};
use crate::binary::{Component, ComponentType};
use sallyport::syscall::{SYS_ENARX_CPUID, SYS_ENARX_ERESUME, SYS_ENARX_GETATT};
use sallyport::Block;

use anyhow::{anyhow, Result};
use lset::{Line, Span};
use primordial::Page;
use sgx::enclave::{Builder, Enclave, Entry, Registers, Segment};
use sgx::types::{
    page::{Flags, SecInfo},
    ssa::Exception,
    tcs::Tcs,
};

use std::arch::x86_64::__cpuid_count;
use std::convert::TryInto;
use std::path::Path;
use std::sync::{Arc, RwLock};

mod attestation;
mod data;
mod shim;
use goblin::elf::program_header::*;
use std::cmp::min;
use std::ops::Range;

const SHIM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bin/shim-sgx"));

fn program_header_2_segment(file: impl AsRef<[u8]>, ph: &ProgramHeader) -> Segment {
    let mut rwx = Flags::empty();

    if ph.is_read() {
        rwx |= Flags::R;
    }
    if ph.is_write() {
        rwx |= Flags::W;
    }
    if ph.is_executable() {
        rwx |= Flags::X;
    }

    let src = Span::from(ph.file_range());

    let unaligned = Line::from(ph.vm_range());

    let frame = Line {
        start: unaligned.start / Page::size(),
        end: (unaligned.end + Page::size() - 1) / Page::size(),
    };

    let aligned = Line {
        start: frame.start * Page::size(),
        end: frame.end * Page::size(),
    };

    let subslice = Span::from(Line {
        start: unaligned.start - aligned.start,
        end: unaligned.end - aligned.start,
    });

    let subslice = Range::from(Span {
        start: subslice.start,
        count: min(subslice.count, src.count),
    });

    let src = &file.as_ref()[Range::from(src)];
    let mut buf = vec![Page::default(); Span::from(frame).count];
    unsafe { buf.align_to_mut() }.1[subslice].copy_from_slice(src);

    Segment {
        si: SecInfo::reg(rwx),
        dst: aligned.start,
        src: buf,
    }
}

pub struct Backend;

impl crate::backend::Backend for Backend {
    fn name(&self) -> &'static str {
        "sgx"
    }

    fn have(&self) -> bool {
        data::dev_sgx_enclave().pass
    }

    fn data(&self) -> Vec<Datum> {
        let mut data = vec![data::dev_sgx_enclave()];

        data.extend(data::CPUIDS.iter().map(|c| c.into()));

        let max = unsafe { __cpuid_count(0x00000000, 0x00000000) }.eax;
        data.push(data::epc_size(max));

        data
    }

    /// Create a keep instance on this backend
    fn build(&self, code: Component, _sock: Option<&Path>) -> Result<Arc<dyn Keep>> {
        let shim = ComponentType::Shim.into_component_from_bytes(SHIM)?;

        // Calculate the memory layout for the enclave.
        let layout = crate::backend::sgx::shim::Layout::calculate(shim.region(), code.region());

        let mut shim_segs: Vec<_> = shim
            .filter_header(PT_LOAD)
            .map(|v| program_header_2_segment(shim.bytes, v))
            .collect();

        let mut code_segs: Vec<_> = code
            .filter_header(PT_LOAD)
            .map(|v| program_header_2_segment(code.bytes, v))
            .collect();

        // Relocate the shim binary.
        let shim_entry = shim.elf.entry as usize + layout.shim.start;

        for seg in shim_segs.iter_mut() {
            seg.dst += layout.shim.start;
        }

        // Relocate the code binary.
        for seg in code_segs.iter_mut() {
            seg.dst += layout.code.start;
        }

        // Create SSAs and TCS.
        let ssas = vec![Page::default(); 3];
        let tcs = Tcs::new(
            shim_entry - layout.enclave.start,
            Page::size() * 2, // SSAs after Layout (see below)
            ssas.len() as _,
        );

        let internal = vec![
            // TCS
            Segment {
                si: SecInfo::tcs(),
                dst: layout.prefix.start,
                src: vec![Page::copy(tcs)],
            },
            // Layout
            Segment {
                si: SecInfo::reg(Flags::R),
                dst: layout.prefix.start + Page::size(),
                src: vec![Page::copy(layout)],
            },
            // SSAs
            Segment {
                si: SecInfo::reg(Flags::R | Flags::W),
                dst: layout.prefix.start + Page::size() * 2,
                src: ssas,
            },
            // Heap
            Segment {
                si: SecInfo::reg(Flags::R | Flags::W | Flags::X),
                dst: layout.heap.start,
                src: vec![Page::default(); Span::from(layout.heap).count / Page::size()],
            },
            // Stack
            Segment {
                si: SecInfo::reg(Flags::R | Flags::W),
                dst: layout.stack.start,
                src: vec![Page::default(); Span::from(layout.stack).count / Page::size()],
            },
        ];

        // Initiate the enclave building process.
        let mut builder = Builder::new(layout.enclave).expect("Unable to create builder");
        builder.load(&internal)?;
        builder.load(&shim_segs)?;
        builder.load(&code_segs)?;
        Ok(builder.build()?)
    }

    fn measure(&self, mut _code: Component) -> Result<String> {
        unimplemented!()
    }
}

impl super::Keep for RwLock<Enclave> {
    fn add_thread(self: Arc<Self>) -> Result<Box<dyn crate::backend::Thread>> {
        Ok(Box::new(Thread {
            thread: sgx::enclave::Thread::new(self).ok_or_else(|| anyhow!("out of threads"))?,
            registers: Registers::default(),
            block: Block::default(),
            cssa: usize::default(),
            how: Entry::Enter,
        }))
    }
}

struct Thread {
    thread: sgx::enclave::Thread,
    registers: Registers,
    block: Block,
    cssa: usize,
    how: Entry,
}

impl Thread {
    fn cpuid(&mut self) {
        unsafe {
            let cpuid = core::arch::x86_64::__cpuid_count(
                self.block.msg.req.arg[0].try_into().unwrap(),
                self.block.msg.req.arg[1].try_into().unwrap(),
            );

            self.block.msg.req.arg[0] = cpuid.eax.into();
            self.block.msg.req.arg[1] = cpuid.ebx.into();
            self.block.msg.req.arg[2] = cpuid.ecx.into();
            self.block.msg.req.arg[3] = cpuid.edx.into();
        }
    }

    fn attest(&mut self) -> Result<()> {
        let result = unsafe {
            get_attestation(
                self.block.msg.req.arg[0].into(),
                self.block.msg.req.arg[1].into(),
                self.block.msg.req.arg[2].into(),
                self.block.msg.req.arg[3].into(),
            )?
        };

        self.block.msg.rep = Ok([result.into(), 0.into()]).into();
        Ok(())
    }
}

impl super::Thread for Thread {
    fn enter(&mut self) -> Result<Command> {
        loop {
            let prev = self.how;
            self.registers.rdx = (&mut self.block).into();
            self.how = match self.thread.enter(prev, &mut self.registers) {
                Err(ei) if ei.trap == Exception::InvalidOpcode => Entry::Enter,
                Ok(_) => Entry::Resume,
                e => panic!("Unexpected AEX: {:?}", e),
            };

            // Keep track of the CSSA
            match self.how {
                Entry::Enter => self.cssa += 1,
                Entry::Resume => match self.cssa {
                    0 => unreachable!(),
                    _ => self.cssa -= 1,
                },
            }

            // If we have handled an InvalidOpcode error, evaluate the sallyport.
            if let (Entry::Enter, Entry::Resume) = (prev, self.how) {
                match unsafe { self.block.msg.req }.num.into() {
                    SYS_ENARX_CPUID => self.cpuid(),
                    SYS_ENARX_GETATT => self.attest()?,
                    SYS_ENARX_ERESUME => (),
                    _ => return Ok(Command::SysCall(&mut self.block)),
                }
            }
        }
    }
}
