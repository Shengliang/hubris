// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::miim_bridge::MiimBridge;

use drv_spi_api::{Spi, SpiError};
use drv_stm32h7_eth as eth;
use drv_stm32xx_sys_api::{self as sys_api, Sys};
use drv_user_leds_api::UserLeds;
use ksz8463::{Ksz8463, MIBCounter, MIBOffset, Register as KszRegister};
use ringbuf::*;
use userlib::{hl::sleep_for, task_slot};
use vsc7448_pac::{phy, types::PhyRegisterAddress};
use vsc85xx::{Phy, VscError};

task_slot!(SPI, spi_driver);
task_slot!(USER_LEDS, user_leds);

const KSZ8463_SPI_DEVICE: u8 = 0; // Based on app.toml ordering
const VSC8552_PORT: u8 = 0b11100; // Based on resistor strapping

#[derive(Copy, Clone, Debug, PartialEq)]
enum Trace {
    None,
    Ksz8463Configured,
    KszErr {
        err: SpiError,
    },
    Ksz8463Status {
        port: u8,
        status: u16,
    },
    Ksz8463Control {
        port: u8,
        control: u16,
    },
    Ksz8463Counter {
        port: u8,
        counter: MIBCounter,
    },
    Ksz8463MacTable(ksz8463::MacTableEntry),

    Vsc8552Configured,
    Vsc8552Status {
        port: u8,
        status: phy::standard::MODE_STATUS,
    },
    Vsc8552MacPcsStatus {
        port: u8,
        status: phy::extended_3::MAC_SERDES_PCS_STATUS,
    },
    Vsc8552MacPcsControl {
        port: u8,
        control: phy::extended_3::MAC_SERDES_PCS_CONTROL,
    },
    Vsc8552MediaSerdesStatus {
        port: u8,
        status: phy::extended_3::MEDIA_SERDES_STATUS,
    },
    Vsc8552Err {
        err: VscError,
    },
    Vsc8552BypassControl {
        port: u8,
        control: phy::standard::BYPASS_CONTROL,
    },
    Vsc8552Status100 {
        port: u8,
        status: u16,
    },
    Vsc8552TxGoodCounter {
        port: u8,
        counter: phy::extended_3::MEDIA_SERDES_TX_GOOD_PACKET_COUNTER,
    },
    Vsc8552RxCRCGoodCounter {
        port: u8,
        counter: phy::extended_3::MEDIA_MAC_SERDES_RX_GOOD_COUNTER,
    },
}
ringbuf!(Trace, 32, Trace::None);

// This system wants to be woken periodically to do logging
pub const WAKE_INTERVAL: Option<u64> = Some(500);

////////////////////////////////////////////////////////////////////////////////

pub struct Bsp {
    ksz: Ksz8463,
    leds: UserLeds,
}

impl Bsp {
    pub fn new() -> Self {
        let spi = Spi::from(SPI.get_task_id()).device(KSZ8463_SPI_DEVICE);
        let ksz = Ksz8463::new(
            spi,
            sys_api::Port::A.pin(9),
            ksz8463::ResetSpeed::Slow,
        );
        let leds = drv_user_leds_api::UserLeds::from(USER_LEDS.get_task_id());

        Self { ksz, leds }
    }

    pub fn configure_ethernet_pins(&self, sys: &Sys) {
        // This board's mapping:
        //
        // RMII REF CLK     PA1
        // RMII RX DV       PA7
        //
        // RMII RXD0        PC4
        // RMII RXD1        PC5
        //
        // RMII TX EN       PG11
        // RMII TXD1        PG12
        // RMII TXD0        PG13
        //
        // MDIO             PA2
        //
        // MDC              PC1
        //
        // (it's _almost_ identical to the STM32H7 Nucleo, except that
        //  TXD1 is on a different pin)
        //
        //  The MDIO/MDC lines run at Speed::Low because otherwise the VSC8504
        //  refuses to talk.
        use sys_api::*;
        let eth_af = Alternate::AF11;

        // RMII
        sys.gpio_configure(
            Port::A,
            (1 << 1) | (1 << 7),
            Mode::Alternate,
            OutputType::PushPull,
            Speed::VeryHigh,
            Pull::None,
            eth_af,
        )
        .unwrap();
        sys.gpio_configure(
            Port::C,
            (1 << 4) | (1 << 5),
            Mode::Alternate,
            OutputType::PushPull,
            Speed::VeryHigh,
            Pull::None,
            eth_af,
        )
        .unwrap();
        sys.gpio_configure(
            Port::G,
            (1 << 11) | (1 << 12) | (1 << 13),
            Mode::Alternate,
            OutputType::PushPull,
            Speed::VeryHigh,
            Pull::None,
            eth_af,
        )
        .unwrap();

        // SMI (MDC and MDIO)
        sys.gpio_configure(
            Port::A,
            1 << 2,
            Mode::Alternate,
            OutputType::PushPull,
            Speed::Low,
            Pull::None,
            eth_af,
        )
        .unwrap();
        sys.gpio_configure(
            Port::C,
            1 << 1,
            Mode::Alternate,
            OutputType::PushPull,
            Speed::Low,
            Pull::None,
            eth_af,
        )
        .unwrap();
    }

    pub fn configure_phy(&self, eth: &mut eth::Ethernet, sys: &Sys) {
        self.leds.led_off(0).unwrap();
        self.leds.led_on(3).unwrap();

        // The KSZ8463 connects to the SP over RMII, then sends data to the
        // VSC8552 over 100-BASE FX
        self.ksz.configure(sys);
        ringbuf_entry!(Trace::Ksz8463Configured);

        // The VSC8552 connects the KSZ switch to the management network
        // over SGMII
        configure_vsc8552(eth, sys);
        ringbuf_entry!(Trace::Vsc8552Configured);

        self.leds.led_on(0).unwrap();
        self.leds.led_off(3).unwrap();
    }

    pub fn wake(&self, eth: &mut eth::Ethernet) {
        // Logging for KSZ8463 port 1
        ringbuf_entry!(match self.ksz.read(KszRegister::P1MBSR) {
            Ok(status) => Trace::Ksz8463Status { port: 1, status },
            Err(err) => Trace::KszErr { err },
        });
        ringbuf_entry!(match self.ksz.read(KszRegister::P1MBCR) {
            Ok(control) => Trace::Ksz8463Control { port: 1, control },
            Err(err) => Trace::KszErr { err },
        });
        ringbuf_entry!(match self
            .ksz
            .read_mib_counter(1, MIBOffset::RxLoPriorityByte)
        {
            Ok(counter) => Trace::Ksz8463Counter { port: 1, counter },
            Err(err) => Trace::KszErr { err },
        });

        // Logging for KSZ8463 port 2
        ringbuf_entry!(match self.ksz.read(KszRegister::P2MBSR) {
            Ok(status) => Trace::Ksz8463Status { port: 2, status },
            Err(err) => Trace::KszErr { err },
        });
        ringbuf_entry!(match self.ksz.read(KszRegister::P2MBCR) {
            Ok(control) => Trace::Ksz8463Control { port: 2, control },
            Err(err) => Trace::KszErr { err },
        });
        ringbuf_entry!(match self
            .ksz
            .read_mib_counter(2, MIBOffset::RxLoPriorityByte)
        {
            Ok(counter) => Trace::Ksz8463Counter { port: 2, counter },
            Err(err) => Trace::KszErr { err },
        });

        // Read the MAC table for fun
        ringbuf_entry!(match self.ksz.read_dynamic_mac_table(0) {
            Ok(mac) => Trace::Ksz8463MacTable(mac),
            Err(err) => Trace::KszErr { err },
        });

        let mut any_comma = false;
        let mut any_link = false;
        for i in [0, 1] {
            let port = VSC8552_PORT + i;
            let mut phy = Phy {
                port,
                rw: &mut MiimBridge::new(eth),
            };

            ringbuf_entry!(match phy.read(phy::STANDARD::MODE_STATUS()) {
                Ok(status) => Trace::Vsc8552Status { port, status },
                Err(err) => Trace::Vsc8552Err { err },
            });

            // This is a non-standard register address
            let extended_status =
                PhyRegisterAddress::<u16>::from_page_and_addr_unchecked(0, 16);
            ringbuf_entry!(match phy.read(extended_status) {
                Ok(status) => Trace::Vsc8552Status100 { port, status },
                Err(err) => Trace::Vsc8552Err { err },
            });

            ringbuf_entry!(match phy.read(phy::STANDARD::BYPASS_CONTROL()) {
                Ok(control) => Trace::Vsc8552BypassControl { port, control },
                Err(err) => Trace::Vsc8552Err { err },
            });

            ringbuf_entry!(match phy
                .read(phy::EXTENDED_3::MEDIA_SERDES_TX_GOOD_PACKET_COUNTER())
            {
                Ok(counter) => Trace::Vsc8552TxGoodCounter { port, counter },
                Err(err) => Trace::Vsc8552Err { err },
            });
            ringbuf_entry!(match phy
                .read(phy::EXTENDED_3::MEDIA_MAC_SERDES_RX_GOOD_COUNTER())
            {
                Ok(counter) => Trace::Vsc8552RxCRCGoodCounter { port, counter },
                Err(err) => Trace::Vsc8552Err { err },
            });
            ringbuf_entry!(match phy
                .read(phy::EXTENDED_3::MAC_SERDES_PCS_STATUS())
            {
                Ok(status) => {
                    any_link |= (status.0 & (1 << 2)) != 0;
                    any_comma |= (status.0 & (1 << 0)) != 0;
                    Trace::Vsc8552MacPcsStatus { port, status }
                }
                Err(err) => Trace::Vsc8552Err { err },
            });
            ringbuf_entry!(match phy
                .read(phy::EXTENDED_3::MEDIA_SERDES_STATUS())
            {
                Ok(status) => Trace::Vsc8552MediaSerdesStatus { port, status },
                Err(err) => Trace::Vsc8552Err { err },
            });
            ringbuf_entry!(match phy
                .read(phy::EXTENDED_3::MAC_SERDES_PCS_CONTROL())
            {
                Ok(control) => {
                    Trace::Vsc8552MacPcsControl { port, control }
                }
                Err(err) => Trace::Vsc8552Err { err },
            });
        }

        if any_link {
            self.leds.led_on(1).unwrap();
        } else {
            self.leds.led_off(1).unwrap();
        }
        if any_comma {
            self.leds.led_on(2).unwrap();
        } else {
            self.leds.led_off(2).unwrap();
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

pub fn configure_vsc8552(eth: &mut eth::Ethernet, sys: &Sys) {
    use sys_api::*;

    let nrst = Port::A.pin(10);

    // Start with reset low
    sys.gpio_reset(nrst).unwrap();
    sys.gpio_configure_output(
        nrst,
        OutputType::PushPull,
        Speed::Low,
        Pull::None,
    )
    .unwrap();
    sleep_for(4);

    sys.gpio_set(nrst).unwrap();
    sleep_for(120); // Wait for the chip to come out of reset

    // The VSC8552 patch must be applied to port 0 in the phy
    let mut phy_rw = MiimBridge::new(eth);
    let mut phy0 = Phy {
        port: VSC8552_PORT,
        rw: &mut phy_rw,
    };
    vsc85xx::patch_vsc8552_phy(&mut phy0).unwrap();

    // Port 0 on the PHY is connected to a SFF-8087 Mini-Sas
    vsc85xx::init_vsc8552_phy(&mut phy0).unwrap();

    // Port 1 on the PHY is connected to SMA connectors
    let mut phy1 = Phy {
        port: VSC8552_PORT + 1,
        rw: &mut phy_rw,
    };
    vsc85xx::init_vsc8552_phy(&mut phy1).unwrap();
}
