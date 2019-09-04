#![no_std]
#![no_main]
#![warn(rust_2018_idioms)]

// We need to import this crate explicitly so we have a panic handler
use panic_semihosting as _;

mod logger;

use {
    bbqueue::{bbq, BBQueue, Consumer},
    byteorder::{ByteOrder, LittleEndian},
    core::fmt::Write,
    cortex_m_semihosting::hprintln,
    nrf52810_hal::{
        self as hal,
        gpio::Level,
        nrf52810_pac::{self as pac, UARTE0},
        prelude::*,
        uarte::{Baudrate, Parity, Uarte},
    },
    rtfm::app,
    rubble::{
        gatt::BatteryServiceAttrs,
        l2cap::{BleChannelMap, L2CAPState},
        link::{
            ad_structure::AdStructure, queue, AddressKind, DeviceAddress, HardwareInterface,
            LinkLayer, Responder, MIN_PDU_BUF,
        },
        security_manager::NoSecurity,
        time::{Duration, Timer},
    },
    rubble_nrf52::{
        radio::{BleRadio, PacketBuffer},
        timer::BleTimer,
    },
};

rubble::include_attributes!(mod attrs);

/// Hardware interface for the BLE stack (nRF52810 implementation).
pub struct HwNRf52810 {}

impl HardwareInterface for HwNRf52810 {
    type Timer = BleTimer<pac::TIMER0>;
    type Tx = BleRadio;
}

#[app(device = nrf52810_hal::nrf52810_pac)]
const APP: () = {
    static mut BLE_TX_BUF: PacketBuffer = [0; MIN_PDU_BUF];
    static mut BLE_RX_BUF: PacketBuffer = [0; MIN_PDU_BUF];
    static mut BLE_LL: LinkLayer<HwNRf52810> = ();
    static mut BLE_R: Responder<BleChannelMap<BatteryServiceAttrs, NoSecurity>> = ();
    static mut RADIO: BleRadio = ();
    static mut SERIAL: Uarte<UARTE0> = ();
    static mut LOG_SINK: Consumer = ();

    #[init(resources = [BLE_TX_BUF, BLE_RX_BUF])]
    fn init() {
        hprintln!("\n<< INIT >>\n").ok();

        {
            // On reset the internal high frequency clock is used, but starting the HFCLK task
            // switches to the external crystal; this is needed for Bluetooth to work.

            device
                .CLOCK
                .tasks_hfclkstart
                .write(|w| unsafe { w.bits(1) });
            while device.CLOCK.events_hfclkstarted.read().bits() == 0 {}
        }

        let ble_timer = BleTimer::init(device.TIMER0);

        let p0 = device.P0.split();

        let mut serial = {
            let rxd = p0.p0_08.into_floating_input().degrade();
            let txd = p0.p0_06.into_push_pull_output(Level::Low).degrade();

            let pins = hal::uarte::Pins {
                rxd,
                txd,
                cts: None,
                rts: None,
            };

            device
                .UARTE0
                .constrain(pins, Parity::EXCLUDED, Baudrate::BAUD1M)
        };
        writeln!(serial, "\n--- INIT ---").unwrap();

        let mut devaddr = [0u8; 6];
        let devaddr_lo = device.FICR.deviceaddr[0].read().bits();
        let devaddr_hi = device.FICR.deviceaddr[1].read().bits() as u16;
        LittleEndian::write_u32(&mut devaddr, devaddr_lo);
        LittleEndian::write_u16(&mut devaddr[4..], devaddr_hi);

        let devaddr_type = if device
            .FICR
            .deviceaddrtype
            .read()
            .deviceaddrtype()
            .is_public()
        {
            AddressKind::Public
        } else {
            AddressKind::Random
        };
        let device_address = DeviceAddress::new(devaddr, devaddr_type);

        let mut radio = BleRadio::new(device.RADIO, resources.BLE_TX_BUF, resources.BLE_RX_BUF);

        let log_sink = logger::init(ble_timer.create_stamp_source());

        // Create TX/RX queues
        // FIXME: Because of how bbqueue works, these have to be 2x the max. PDU size. We don't need
        // contiguous segments though, so we could use a "normal" queue instead.
        let (tx, tx_cons) = queue::create(bbq![MIN_PDU_BUF * 2].unwrap());
        let (rx_prod, rx) = queue::create(bbq![MIN_PDU_BUF * 2].unwrap());

        // Create the actual BLE stack objects
        let mut ll = LinkLayer::<HwNRf52810>::new(device_address, ble_timer);

        let resp = Responder::new(
            tx,
            rx,
            L2CAPState::new(BleChannelMap::with_attributes(BatteryServiceAttrs::new())),
        );

        // Send advertisement and set up regular interrupt
        let next_update = ll
            .start_advertise(
                Duration::from_millis(200),
                &[AdStructure::CompleteLocalName("CONCVRRENS CERTA CELERIS")],
                &mut radio,
                tx_cons,
                rx_prod,
            )
            .unwrap();
        ll.timer().configure_interrupt(next_update);

        RADIO = radio;
        BLE_LL = ll;
        BLE_R = resp;
        SERIAL = serial;
        LOG_SINK = log_sink;
    }

    #[interrupt(resources = [RADIO, BLE_LL])]
    fn RADIO() {
        let next_update = resources
            .RADIO
            .recv_interrupt(resources.BLE_LL.timer().now(), &mut resources.BLE_LL);
        resources.BLE_LL.timer().configure_interrupt(next_update);
    }

    #[interrupt(resources = [RADIO, BLE_LL])]
    fn TIMER0() {
        let timer = resources.BLE_LL.timer();
        if !timer.is_interrupt_pending() {
            return;
        }
        timer.clear_interrupt();

        let cmd = resources.BLE_LL.update(&mut *resources.RADIO);
        resources.RADIO.configure_receiver(cmd.radio);

        resources
            .BLE_LL
            .timer()
            .configure_interrupt(cmd.next_update);
    }

    #[idle(resources = [LOG_SINK, SERIAL, BLE_R])]
    fn idle() -> ! {
        // Drain the logging buffer through the serial connection
        loop {
            if cfg!(feature = "log") {
                while let Ok(grant) = resources.LOG_SINK.read() {
                    for chunk in grant.buf().chunks(255) {
                        resources.SERIAL.write(chunk).unwrap();
                    }

                    resources.LOG_SINK.release(grant.buf().len(), grant);
                }
            }

            if resources.BLE_R.has_work() {
                resources.BLE_R.process_one().unwrap();
            }
        }
    }
};
