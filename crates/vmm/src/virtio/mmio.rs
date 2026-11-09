//! The virtio-MMIO transport, version 2. The guest picks every offset, width,
//! and value, so an access that means nothing is ignored.

use super::queue::Queue;

const MAGIC: u32 = 0x7472_6976;
const VERSION: u32 = 2;
const VERSION_1: u64 = 1 << 32;
const FEATURES_OK: u32 = 8;
const USED_INTERRUPT: u32 = 1;
const CONFIG: u64 = 0x100;

/// The registers of one virtio-MMIO device.
pub struct Transport {
    device_id: u32,
    features: u64,
    config: Vec<u8>,
    queues: Vec<Queue>,
    device_features_select: u32,
    driver_features: u64,
    driver_features_select: u32,
    queue_select: u32,
    interrupt_status: u32,
    status: u32,
}

impl Transport {
    /// A device of type `device_id` with `queues` queues of up to `max_size`
    /// entries. `config` is its read-only configuration space.
    pub fn new(
        device_id: u32,
        features: u64,
        config: Vec<u8>,
        queues: usize,
        max_size: u16,
    ) -> Self {
        Self {
            device_id,
            features: features | VERSION_1,
            config,
            queues: (0..queues).map(|_| Queue::new(max_size)).collect(),
            device_features_select: 0,
            driver_features: 0,
            driver_features_select: 0,
            queue_select: 0,
            interrupt_status: 0,
            status: 0,
        }
    }

    /// A queue, for the device to work on.
    pub fn queue(&mut self, index: usize) -> Option<&mut Queue> {
        self.queues.get_mut(index)
    }

    /// Records that the device put something on a used ring.
    pub fn signal_used(&mut self) {
        self.interrupt_status |= USED_INTERRUPT;
    }

    /// Handles a guest read. `data` is zero unless the access means something.
    pub fn read(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if offset >= CONFIG {
            self.read_config(offset - CONFIG, data);
        } else if let Some(register) = register(offset, data.len()) {
            data.copy_from_slice(&self.read_register(register).to_le_bytes());
        }
    }

    /// Handles a guest write. Returns the index of a queue the guest notified.
    pub fn write(&mut self, offset: u64, data: &[u8]) -> Option<u16> {
        let register = register(offset, data.len())?;
        let value = u32::from_le_bytes(data.try_into().ok()?);
        self.write_register(register, value)
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        let bytes = usize::try_from(offset).ok().and_then(|start| {
            let end = start.checked_add(data.len())?;
            self.config.get(start..end)
        });
        if let Some(bytes) = bytes {
            data.copy_from_slice(bytes);
        }
    }

    fn read_register(&self, register: u64) -> u32 {
        let queue = self.queues.get(self.queue_select as usize);
        match register {
            0x000 => MAGIC,
            0x004 => VERSION,
            0x008 => self.device_id,
            0x010 => half(self.features, self.device_features_select),
            0x034 => queue.map_or(0, |queue| u32::from(queue.max_size)),
            0x044 => queue.map_or(0, |queue| u32::from(queue.ready)),
            0x060 => self.interrupt_status,
            0x070 => self.status,
            _ => 0,
        }
    }

    fn write_register(&mut self, register: u64, value: u32) -> Option<u16> {
        match register {
            0x014 => self.device_features_select = value,
            0x020 => self.write_driver_features(value),
            0x024 => self.driver_features_select = value,
            0x030 => self.queue_select = value,
            0x044 => self.write_queue_ready(value),
            0x050 => return self.notified(value),
            0x064 => self.interrupt_status &= !value,
            0x070 => self.write_status(value),
            _ => self.write_queue_setup(register, value),
        }
        None
    }

    fn write_driver_features(&mut self, value: u32) {
        match self.driver_features_select {
            0 => set_low(&mut self.driver_features, value),
            1 => set_high(&mut self.driver_features, value),
            _ => {}
        }
    }

    fn write_queue_ready(&mut self, value: u32) {
        if let Some(queue) = self.queues.get_mut(self.queue_select as usize) {
            queue.ready = value != 0 && queue.size_is_valid();
        }
    }

    fn notified(&self, value: u32) -> Option<u16> {
        let index = u16::try_from(value).ok()?;
        (usize::from(index) < self.queues.len()).then_some(index)
    }

    fn write_status(&mut self, value: u32) {
        if value == 0 {
            self.reset();
        } else if self.driver_features & !self.features != 0 {
            self.status = value & !FEATURES_OK;
        } else {
            self.status = value;
        }
    }

    fn reset(&mut self) {
        self.queues.iter_mut().for_each(Queue::reset);
        self.device_features_select = 0;
        self.driver_features = 0;
        self.driver_features_select = 0;
        self.queue_select = 0;
        self.interrupt_status = 0;
        self.status = 0;
    }

    /// A ready queue's setup can't change, so a device never sees it move.
    fn write_queue_setup(&mut self, register: u64, value: u32) {
        let Some(queue) = self.queues.get_mut(self.queue_select as usize) else {
            return;
        };
        if queue.ready {
            return;
        }
        match register {
            // A size too big for 16 bits becomes 0, which can't be made ready.
            0x038 => queue.size = u16::try_from(value).unwrap_or(0),
            0x080 => set_low(&mut queue.descriptors, value),
            0x084 => set_high(&mut queue.descriptors, value),
            0x090 => set_low(&mut queue.available, value),
            0x094 => set_high(&mut queue.available, value),
            0x0a0 => set_low(&mut queue.used, value),
            0x0a4 => set_high(&mut queue.used, value),
            _ => {}
        }
    }
}

/// `offset`, if it is a whole 32-bit register access.
fn register(offset: u64, width: usize) -> Option<u64> {
    (offset < CONFIG && width == 4 && offset % 4 == 0).then_some(offset)
}

/// Half of a 64-bit feature set, or zero for a select with no half.
fn half(value: u64, select: u32) -> u32 {
    match select {
        0 => value as u32,
        1 => (value >> 32) as u32,
        _ => 0,
    }
}

fn set_low(target: &mut u64, value: u32) {
    *target = (*target & !0xffff_ffff) | u64::from(value);
}

fn set_high(target: &mut u64, value: u32) {
    *target = (*target & 0xffff_ffff) | (u64::from(value) << 32);
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAGIC: u64 = 0x000;
    const VERSION: u64 = 0x004;
    const DEVICE_ID: u64 = 0x008;
    const DEVICE_FEATURES: u64 = 0x010;
    const DEVICE_FEATURES_SELECT: u64 = 0x014;
    const DRIVER_FEATURES: u64 = 0x020;
    const DRIVER_FEATURES_SELECT: u64 = 0x024;
    const QUEUE_SELECT: u64 = 0x030;
    const QUEUE_NUM_MAX: u64 = 0x034;
    const QUEUE_NUM: u64 = 0x038;
    const QUEUE_READY: u64 = 0x044;
    const QUEUE_NOTIFY: u64 = 0x050;
    const INTERRUPT_STATUS: u64 = 0x060;
    const INTERRUPT_ACK: u64 = 0x064;
    const STATUS: u64 = 0x070;
    const QUEUE_DESC_LOW: u64 = 0x080;
    const QUEUE_DESC_HIGH: u64 = 0x084;
    const QUEUE_DRIVER_LOW: u64 = 0x090;
    const QUEUE_DEVICE_LOW: u64 = 0x0a0;
    const CONFIG: u64 = 0x100;
    const FEATURES_OK: u32 = 8;
    const FEATURE: u64 = 1 << 3;
    const VERSION_1_HIGH: u32 = 1;

    fn device() -> Transport {
        Transport::new(19, FEATURE, vec![1, 2, 3, 4, 5, 6, 7, 8], 2, 8)
    }

    fn get(device: &Transport, offset: u64) -> u32 {
        let mut data = [0xff; 4];
        device.read(offset, &mut data);
        u32::from_le_bytes(data)
    }

    fn set(device: &mut Transport, offset: u64, value: u32) -> Option<u16> {
        device.write(offset, &value.to_le_bytes())
    }

    fn ready(device: &mut Transport) {
        set(device, QUEUE_NUM, 8);
        set(device, QUEUE_READY, 1);
    }

    #[test]
    fn the_guest_can_identify_the_device() {
        let device = device();
        assert_eq!(get(&device, MAGIC), 0x7472_6976);
        assert_eq!(get(&device, VERSION), 2);
        assert_eq!(get(&device, DEVICE_ID), 19);
    }

    #[test]
    fn features_read_a_half_at_a_time_and_include_version_1() {
        let mut device = device();
        assert_eq!(get(&device, DEVICE_FEATURES), FEATURE as u32);
        set(&mut device, DEVICE_FEATURES_SELECT, 1);
        assert_eq!(get(&device, DEVICE_FEATURES), VERSION_1_HIGH);
    }

    #[test]
    fn a_wild_feature_select_reads_zero_and_doesnt_panic() {
        let mut device = device();
        for select in [2, 64, u32::MAX] {
            set(&mut device, DEVICE_FEATURES_SELECT, select);
            assert_eq!(get(&device, DEVICE_FEATURES), 0);
            set(&mut device, DRIVER_FEATURES_SELECT, select);
            set(&mut device, DRIVER_FEATURES, u32::MAX);
        }
        set(&mut device, STATUS, FEATURES_OK);
        assert_eq!(get(&device, STATUS), FEATURES_OK);
    }

    #[test]
    fn features_the_device_offers_are_accepted() {
        let mut device = device();
        set(&mut device, DRIVER_FEATURES, FEATURE as u32);
        set(&mut device, DRIVER_FEATURES_SELECT, 1);
        set(&mut device, DRIVER_FEATURES, VERSION_1_HIGH);
        set(&mut device, STATUS, FEATURES_OK);
        assert_eq!(get(&device, STATUS), FEATURES_OK);
    }

    #[test]
    fn features_the_device_doesnt_offer_are_refused() {
        let mut device = device();
        set(&mut device, DRIVER_FEATURES, 1 << 4);
        set(&mut device, STATUS, FEATURES_OK | 1);
        assert_eq!(get(&device, STATUS), 1);
    }

    #[test]
    fn queue_registers_use_the_selected_queue() {
        let mut device = device();
        set(&mut device, QUEUE_SELECT, 1);
        ready(&mut device);
        assert_eq!(get(&device, QUEUE_READY), 1);
        set(&mut device, QUEUE_SELECT, 0);
        assert_eq!(get(&device, QUEUE_READY), 0);
        assert_eq!(get(&device, QUEUE_NUM_MAX), 8);
    }

    #[test]
    fn a_queue_that_doesnt_exist_reads_zero_and_ignores_writes() {
        let mut device = device();
        set(&mut device, QUEUE_SELECT, 2);
        assert_eq!(get(&device, QUEUE_NUM_MAX), 0);
        ready(&mut device);
        assert_eq!(get(&device, QUEUE_READY), 0);
        set(&mut device, QUEUE_DESC_LOW, 1);
        set(&mut device, QUEUE_SELECT, u32::MAX);
        ready(&mut device);
        assert_eq!(get(&device, QUEUE_READY), 0);
    }

    #[test]
    fn a_bad_queue_size_cant_become_ready() {
        for size in [0, 3, 16, 65536 + 8, u32::MAX] {
            let mut device = device();
            set(&mut device, QUEUE_NUM, size);
            set(&mut device, QUEUE_READY, 1);
            assert_eq!(get(&device, QUEUE_READY), 0, "size {size}");
        }
    }

    #[test]
    fn a_queue_addresses_arrive_in_halves() {
        let mut device = device();
        set(&mut device, QUEUE_DESC_LOW, 0x1000);
        set(&mut device, QUEUE_DESC_HIGH, 0x2);
        set(&mut device, QUEUE_DRIVER_LOW, 0x3000);
        set(&mut device, QUEUE_DEVICE_LOW, 0x4000);
        set(&mut device, QUEUE_DESC_LOW, 0x5000);
        let queue = device.queue(0).unwrap();
        assert_eq!(queue.descriptors, 0x2_0000_5000);
        assert_eq!(queue.available, 0x3000);
        assert_eq!(queue.used, 0x4000);
    }

    #[test]
    fn a_ready_queue_cant_be_moved_or_resized() {
        let mut device = device();
        set(&mut device, QUEUE_DESC_LOW, 0x1000);
        ready(&mut device);
        set(&mut device, QUEUE_DESC_LOW, 0x2000);
        set(&mut device, QUEUE_NUM, 2);
        let queue = device.queue(0).unwrap();
        assert_eq!(queue.descriptors, 0x1000);
        assert_eq!(queue.size, 8);
    }

    #[test]
    fn a_queue_can_stop_being_ready() {
        let mut device = device();
        ready(&mut device);
        set(&mut device, QUEUE_READY, 0);
        assert_eq!(get(&device, QUEUE_READY), 0);
    }

    #[test]
    fn a_notify_names_only_queues_that_exist() {
        let mut device = device();
        assert_eq!(set(&mut device, QUEUE_NOTIFY, 1), Some(1));
        assert_eq!(set(&mut device, QUEUE_NOTIFY, 2), None);
        assert_eq!(set(&mut device, QUEUE_NOTIFY, u32::MAX), None);
    }

    #[test]
    fn an_interrupt_stays_until_it_is_acknowledged() {
        let mut device = device();
        device.signal_used();
        assert_eq!(get(&device, INTERRUPT_STATUS), 1);
        set(&mut device, INTERRUPT_ACK, 2);
        assert_eq!(get(&device, INTERRUPT_STATUS), 1);
        set(&mut device, INTERRUPT_ACK, 1);
        assert_eq!(get(&device, INTERRUPT_STATUS), 0);
    }

    #[test]
    fn a_status_of_zero_resets_the_device() {
        let mut device = device();
        set(&mut device, QUEUE_DESC_LOW, 0x1000);
        ready(&mut device);
        device.signal_used();
        set(&mut device, DEVICE_FEATURES_SELECT, 1);
        set(&mut device, DRIVER_FEATURES, 1 << 4);
        set(&mut device, QUEUE_SELECT, 1);
        set(&mut device, STATUS, 1);
        set(&mut device, STATUS, 0);
        assert_eq!(get(&device, STATUS), 0);
        assert_eq!(get(&device, INTERRUPT_STATUS), 0);
        assert_eq!(get(&device, QUEUE_READY), 0);
        assert_eq!(get(&device, DEVICE_FEATURES), FEATURE as u32);
        assert_eq!(device.queue(0).unwrap().descriptors, 0);
        set(&mut device, STATUS, FEATURES_OK);
        assert_eq!(get(&device, STATUS), FEATURES_OK);
    }

    #[test]
    fn config_reads_at_any_width() {
        let device = device();
        let mut one = [0];
        device.read(CONFIG + 2, &mut one);
        assert_eq!(one, [3]);
        let mut eight = [0; 8];
        device.read(CONFIG, &mut eight);
        assert_eq!(eight, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn config_reads_past_the_end_are_zero() {
        let device = device();
        let mut data = [9; 4];
        device.read(CONFIG + 6, &mut data);
        assert_eq!(data, [0; 4]);
        device.read(u64::MAX, &mut data);
        assert_eq!(data, [0; 4]);
    }

    #[test]
    fn config_is_read_only() {
        let mut device = device();
        assert_eq!(set(&mut device, CONFIG, 0), None);
        let mut data = [0; 4];
        device.read(CONFIG, &mut data);
        assert_eq!(data, [1, 2, 3, 4]);
    }

    #[test]
    fn a_register_access_of_the_wrong_shape_means_nothing() {
        let mut device = device();
        let mut narrow = [9; 2];
        device.read(MAGIC, &mut narrow);
        assert_eq!(narrow, [0; 2]);
        let mut wide = [9; 8];
        device.read(MAGIC, &mut wide);
        assert_eq!(wide, [0; 8]);
        let mut crooked = [9; 4];
        device.read(MAGIC + 1, &mut crooked);
        assert_eq!(crooked, [0; 4]);
        assert_eq!(device.write(QUEUE_NOTIFY, &[0]), None);
        assert_eq!(device.write(QUEUE_NOTIFY + 1, &[0; 4]), None);
    }

    #[test]
    fn an_unknown_register_reads_zero_and_ignores_writes() {
        let mut device = device();
        assert_eq!(get(&device, 0x0f0), 0);
        assert_eq!(set(&mut device, 0x0f0, 1), None);
        assert_eq!(get(&device, MAGIC), 0x7472_6976);
    }
}
