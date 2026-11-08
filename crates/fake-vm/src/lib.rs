//! A `Vm` that runs nothing, and records what it was asked to run.

use ap::Vm;
use std::io;

pub struct FakeVm {
    exit_code: i32,
    commands: Vec<Vec<String>>,
}

impl FakeVm {
    pub fn exiting_with(exit_code: i32) -> FakeVm {
        FakeVm {
            exit_code,
            commands: Vec::new(),
        }
    }

    pub fn commands(&self) -> &[Vec<String>] {
        &self.commands
    }
}

impl Vm for FakeVm {
    fn run(&mut self, command: &[&str]) -> io::Result<i32> {
        self.commands
            .push(command.iter().map(ToString::to_string).collect());
        Ok(self.exit_code)
    }
}
