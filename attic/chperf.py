from subprocess import run
from pathlib import Path
import itertools
import sys

ch = str(Path('../cloud-hypervisor/target/x86_64-unknown-linux-musl/profiling/cloud-hypervisor').resolve())
group = 'chaml'

funcs = [
  '_ZN11acpi_tables3aml49_$LT$impl$u20$acpi_tables..Aml$u20$for$u20$u8$GT$12to_aml_bytes17hc7b5465092900902E',
  '_ZN11acpi_tables3aml50_$LT$impl$u20$acpi_tables..Aml$u20$for$u20$u16$GT$12to_aml_bytes17hcd512bf8974793fbE',
  '_ZN11acpi_tables3aml50_$LT$impl$u20$acpi_tables..Aml$u20$for$u20$u32$GT$12to_aml_bytes17hab6ce64640da050fE',
  '_ZN11acpi_tables3aml50_$LT$impl$u20$acpi_tables..Aml$u20$for$u20$u64$GT$12to_aml_bytes17h1b412693412cd2d1E',
  '_ZN11acpi_tables3aml52_$LT$impl$u20$acpi_tables..Aml$u20$for$u20$usize$GT$12to_aml_bytes17h670e93dd04f80979E',
  '_ZN11acpi_tables3aml54_$LT$impl$u20$acpi_tables..Aml$u20$for$u20$$RF$str$GT$12to_aml_bytes17h40fe958944e083d2E',
  '_ZN50_$LT$vmm..cpu..Cpu$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h10d5fe8da08c7890E',
  '_ZN56_$LT$vmm..cpu..CpuNotify$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hf4c7a776d090cc70E',
  '_ZN57_$LT$acpi_tables..aml..IO$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hf27e446c1d6ad7cfE',
  '_ZN57_$LT$acpi_tables..aml..If$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hd16f05b94ce164baE',
  '_ZN57_$LT$vmm..cpu..CpuMethods$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17ha51e9280697a2b11E',
  '_ZN58_$LT$acpi_tables..aml..Add$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h3ffdc35cc660dd18E',
  '_ZN58_$LT$acpi_tables..aml..And$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h7300ff09b617dd0cE',
  '_ZN58_$LT$acpi_tables..aml..Arg$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h1087d25a60a8d217E',
  '_ZN58_$LT$acpi_tables..aml..One$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hba87c10b9295918dE',
  '_ZN59_$LT$acpi_tables..aml..Name$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h61b4cccbc82c2730E',
  '_ZN59_$LT$acpi_tables..aml..Path$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h31553157280a48abE',
  '_ZN59_$LT$acpi_tables..aml..Zero$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17ha0ef9fb79c6b7d37E',
  '_ZN60_$LT$acpi_tables..aml..Equal$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h5ae19113535f3364E',
  '_ZN60_$LT$acpi_tables..aml..Field$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h402253a9338028f0E',
  '_ZN60_$LT$acpi_tables..aml..Local$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h41379ce915ebb08dE',
  '_ZN60_$LT$acpi_tables..aml..Mutex$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h48a9044430603f2aE',
  '_ZN60_$LT$acpi_tables..aml..Store$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hdb1c5d4fe4379d30E',
  '_ZN60_$LT$acpi_tables..aml..While$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hdae3bdbb90316e36E',
  '_ZN61_$LT$acpi_tables..aml..Device$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h9eaf94d63642df01E',
  '_ZN61_$LT$acpi_tables..aml..Method$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h11edb6e7a7164d73E',
  '_ZN61_$LT$acpi_tables..aml..Notify$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h12d63dd4ea614955E',
  '_ZN61_$LT$acpi_tables..aml..Return$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hb8fd31f7afbe11ccE',
  '_ZN62_$LT$acpi_tables..aml..Acquire$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h5af7829065df7a9bE',
  '_ZN62_$LT$acpi_tables..aml..Package$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h83e371f41e11a795E',
  '_ZN62_$LT$acpi_tables..aml..Release$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h3f80ee21d18be886E',
  '_ZN63_$LT$acpi_tables..aml..EISAName$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hc0b96c9715b3ac0aE',
  '_ZN63_$LT$acpi_tables..aml..LessThan$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h4e668738440e1517E',
  '_ZN63_$LT$acpi_tables..aml..OpRegion$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h980359c866b10a6cE',
  '_ZN63_$LT$acpi_tables..aml..Subtract$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h44e8015b5fbce456E',
  '_ZN64_$LT$acpi_tables..aml..Interrupt$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h74ee6eb14628c807E',
  '_ZN64_$LT$acpi_tables..aml..ShiftLeft$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17ha8952b756d4c0dcaE',
  '_ZN65_$LT$acpi_tables..aml..BufferData$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h5a3c694458fadd0bE',
  '_ZN65_$LT$acpi_tables..aml..MethodCall$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17ha9cff1c72e73bd4bE',
  '_ZN65_$LT$vmm..pci_segment..PciDevSlot$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hd0dfd53e2b107eabE',
  '_ZN67_$LT$vmm..pci_segment..PciDsmMethod$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h3270863991dee19bE',
  '_ZN68_$LT$acpi_tables..aml..Memory32Fixed$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h1e110be3c561dbf3E',
  '_ZN69_$LT$vmm..memory_manager..MemorySlots$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hb8bf6904652895a7E',
  '_ZN70_$LT$vmm..memory_manager..MemoryNotify$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hc4c44ab1dd96074bE',
  '_ZN71_$LT$acpi_tables..aml..CreateDWordField$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h88ea16c05d8017e5E',
  '_ZN71_$LT$acpi_tables..aml..CreateQWordField$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h64d1f79c364f49ceE',
  '_ZN71_$LT$acpi_tables..aml..ResourceTemplate$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hd68eace74fcecd60E',
  '_ZN71_$LT$vmm..device_manager..DeviceManager$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h145be8c8d50e637bE',
  '_ZN71_$LT$vmm..memory_manager..MemoryMethods$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hf67a97296bf2e2eeE',
  '_ZN71_$LT$vmm..pci_segment..PciDevSlotNotify$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h0399b29273414c76E',
  '_ZN72_$LT$vmm..pci_segment..PciDevSlotMethods$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h5d6a44ae4ed23365E',
  '_ZN78_$LT$acpi_tables..aml..AddressSpace$LT$u16$GT$$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17h83a600c229f6e246E',
  '_ZN78_$LT$acpi_tables..aml..AddressSpace$LT$u64$GT$$u20$as$u20$acpi_tables..Aml$GT$12to_aml_bytes17hfa8ad483e3c67c78E',
]


def delete():
    run(['perf', 'probe', '-d', 'chaml:*'], check=True)

def add():
    for i, func in enumerate(funcs):
        name = f'aml_{i:03d}'
        probe = f'{group}:{name}={func}'
        rprobe = f'{group}:r{name}={func}%return'
        run(['perf', 'probe', '-x', ch, '--add', probe], check=True)
        run(['perf', 'probe', '-x', ch, '--add', rprobe], check=True)

def flatten(it): return list(itertools.chain.from_iterable(it))
def record():
    k = '/home/andrew/Repos/linux/vmlinux'
    events = flatten([('-e', f'{group}:aml_{i:03d}') for i, _ in enumerate(funcs)])
    run(['perf', 'record'] + events + [
         #'-e', 'chaml:aml051',
         #'-e', 'chaml:aml050',
         ch,
         '--seccomp', 'log',
         '--kernel', k,
         '--initramfs', 'initramfs',
         '--cpus', 'boot=1',
         '--memory', 'size=1024M',
         ], check=True)

actions = {'add': add, 'record': record, 'delete': delete}
actions[sys.argv[1]]()

# postmortem, this didn't work with the return probe, was getting a could not find symbol error
