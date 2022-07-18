//use std::collections::HashSet;
use std::convert::TryInto;
use std::io::{Error as IOError, Read, Stdin, Stdout, Write};
use std::sync::mpsc::{channel, Receiver};
use std::thread::spawn;

use gdbstub::arch::{Arch, RegId, Registers};
use gdbstub::target::ext::base::singlethread::{SingleThreadOps, StopReason};
use gdbstub::target::ext::base::{BaseOps, ResumeAction};
#[allow(unused)]
use gdbstub::target::ext::breakpoints::{BreakpointsOps, HwBreakpoint, HwBreakpointOps};
use gdbstub::target::{Target, TargetResult};
use gdbstub::Connection;

use crate::{memory, resource, FastModelIris};

pub struct IrisGdbStub<'i> {
    pub iris: &'i mut FastModelIris,
    pub instance_id: u32,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct GuestState {
    pub regs: [u32; 26],
}

impl<'i> IrisGdbStub<'i> {
    pub fn from_instance(iris: &'i mut FastModelIris, instance_id: u32) -> Self {
        Self { iris, instance_id }
    }
}

impl Registers for GuestState {
    type ProgramCounter = u32;
    fn pc(&self) -> u32 {
        self.regs[15]
    }
    fn gdb_serialize(&self, mut write_byte: impl FnMut(Option<u8>)) {
        for (num, reg) in self.regs.iter().enumerate() {
            for byte in reg.to_le_bytes().iter() {
                write_byte(Some(*byte));
            }
            // Registers above 16 and below 24 are assumed to be 96 bit by gdb.
            // So we pad them
            if num >= 16 && num < 24 {
                for _ in 0..8 {
                    write_byte(Some(0));
                }
            }
        }
    }
    fn gdb_deserialize(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if bytes.len() % 4 != 0 {
            return Err(());
        }
        let mut regs = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()));
        for reg in &mut self.regs {
            *reg = regs.next().ok_or(())?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Register {
    R0,
    R1,
    R2,
    R3,
    R4,
    R5,
    R6,
    R7,
    R8,
    R9,
    R10,
    R11,
    R12,
    SP,
    LR,
    PC,
    XPSR,
}

impl RegId for Register {
    fn from_raw_id(id: usize) -> Option<(Self, usize)> {
        use Register::*;
        Some(match id {
            0 => R0,
            1 => R1,
            2 => R2,
            3 => R3,
            4 => R4,
            5 => R5,
            6 => R6,
            7 => R7,
            8 => R8,
            9 => R9,
            10 => R10,
            11 => R11,
            12 => R12,
            13 => SP,
            14 => LR,
            15 => PC,
            25 => XPSR,
            _ => return None,
        })
        .map(|r| (r, 0))
    }
}

impl<'i> Target for IrisGdbStub<'i> {
    type Arch = Armv7mArch;
    type Error = ();
    fn base_ops(&mut self) -> BaseOps<'_, Self::Arch, Self::Error> {
        BaseOps::SingleThread(self)
    }
}

impl SingleThreadOps for IrisGdbStub<'_> {
    fn read_registers(&mut self, regs: &mut GuestState) -> TargetResult<(), Self> {
        for res in
            resource::get_list(&mut self.iris, self.instance_id, None, None).map_err(|_| ())?
        {
            let regnum = match res.name.as_str() {
                "R0" => 0,
                "R1" => 1,
                "R2" => 2,
                "R3" => 3,
                "R4" => 4,
                "R5" => 5,
                "R6" => 6,
                "R7" => 7,
                "R8" => 8,
                "R9" => 9,
                "R10" => 10,
                "R11" => 11,
                "R12" => 12,
                "R13" => 13,
                "R14" => 14,
                "R15" => 15,
                "XPSR" => 25,
                _ => continue,
            };
            let val =
                resource::read(&mut self.iris, self.instance_id, vec![res.id]).map_err(|_| ())?;
            if !val.data.is_empty() {
                regs.regs[regnum] = val.data[0] as u32
            }
        }
        Ok(())
    }

    fn read_addrs(&mut self, start_addr: u32, data: &mut [u8]) -> TargetResult<(), Self> {
        let mem = memory::read(
            &mut self.iris,
            self.instance_id,
            0,
            start_addr as u64,
            1,
            data.len() as u64,
        )
        .map_err(|_| ())?;
        for (offset, byte) in mem
            .data
            .into_iter()
            .map(|u| u.to_le_bytes())
            .flatten()
            .enumerate()
        {
            if data.len() > offset {
                data[offset] = byte;
            }
        }
        Ok(())
    }

    fn write_addrs(&mut self, _: u32, _: &[u8]) -> TargetResult<(), Self> {
        Ok(())
    }
    fn write_registers(&mut self, _: &GuestState) -> TargetResult<(), Self> {
        // We don't support writing
        Ok(())
    }

    fn resume(
        &mut self,
        _: ResumeAction,
        _: gdbstub::target::ext::base::GdbInterrupt<'_>,
    ) -> Result<StopReason<u32>, ()> {
        todo!()
    }
}

pub enum Armv7mArch {}
impl Arch for Armv7mArch {
    type Usize = u32;
    type Registers = GuestState;
    type RegId = Register;
    type BreakpointKind = usize;
}

pub struct GdbOverPipe {
    rx: Receiver<Result<u8, IOError>>,
    write: Stdout,
}

impl<'a> GdbOverPipe {
    pub fn new(read: Stdin, write: Stdout) -> Self {
        let (tx, rx) = channel();
        spawn(move || {
            let mut byte = [0u8];
            let mut read = read;
            loop {
                match read.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => tx.send(Ok(byte[0])).unwrap(),
                    Err(error) => tx.send(Err(error)).unwrap(),
                }
            }
        });
        Self { rx, write }
    }
}

impl Connection for GdbOverPipe {
    type Error = IOError;
    fn write(&mut self, byte: u8) -> Result<(), Self::Error> {
        let outbuf = [byte; 1];
        self.write.write(&outbuf)?;
        self.write.flush()?;
        Ok(())
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.write.flush()
    }
    fn read(&mut self) -> Result<u8, Self::Error> {
        self.rx.recv().unwrap()
    }
    fn peek(&mut self) -> Result<Option<u8>, Self::Error> {
        match self.rx.try_recv() {
            Ok(res) => res.map(Some),
            Err(_) => Ok(None),
        }
    }
}