use ap::Vm;
use fake_vm::FakeVm;

#[test]
fn run_returns_the_exit_code_it_was_given() {
    let mut vm = FakeVm::exiting_with(7);
    assert_eq!(vm.run(&["true"]).unwrap(), 7);
}

#[test]
fn run_records_each_command_in_order() {
    let mut vm = FakeVm::exiting_with(0);
    vm.run(&["claude", "--version"]).unwrap();
    vm.run(&["true"]).unwrap();
    assert_eq!(vm.commands(), [vec!["claude", "--version"], vec!["true"]]);
}
