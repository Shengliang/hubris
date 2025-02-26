// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use drv_stm32xx_sys_api::{self as sys_api, Sys};
use ringbuf::*;
use userlib::{hl::sleep_for, task_slot};
use vsc7448::{Vsc7448, Vsc7448Rw, VscError};
use vsc7448_pac::{phy, types::PhyRegisterAddress};
use vsc85xx::{init_vsc8504_phy, Phy, PhyRw};

task_slot!(SYS, sys);
task_slot!(NET, net);

#[derive(Copy, Clone, PartialEq)]
enum Trace {
    None,
    Vsc8504StatusLink { port: u8, status: u16 },
    Vsc8504Status100Base { port: u8, status: u16 },
}
ringbuf!(Trace, 16, Trace::None);

pub struct Bsp<'a, R> {
    vsc7448: &'a Vsc7448<'a, R>,
    net: task_net_api::Net,
}

impl<'a, R> PhyRw for Bsp<'a, R> {
    fn read_raw<T: From<u16>>(
        &mut self,
        port: u8,
        reg: PhyRegisterAddress<T>,
    ) -> Result<T, VscError> {
        self.net
            .smi_read(port, reg.addr)
            .map(|r| r.into())
            .map_err(|e| e.into())
    }

    fn write_raw<T>(
        &mut self,
        port: u8,
        reg: PhyRegisterAddress<T>,
        value: T,
    ) -> Result<(), VscError>
    where
        u16: From<T>,
        T: From<u16> + Clone,
    {
        self.net
            .smi_write(port, reg.addr, value.into())
            .map_err(|e| e.into())
    }
}

pub fn preinit() {
    // Nothing to do here
}

impl<'a, R: Vsc7448Rw> Bsp<'a, R> {
    /// Constructs and initializes a new BSP handle
    pub fn new(vsc7448: &'a Vsc7448<'a, R>) -> Result<Self, VscError> {
        let net = task_net_api::Net::from(NET.get_task_id());
        let mut out = Bsp { vsc7448, net };
        out.init()?;
        Ok(out)
    }

    fn init(&mut self) -> Result<(), VscError> {
        // Get a handle to modify GPIOs
        let sys = SYS.get_task_id();
        let sys = Sys::from(sys);

        // See RFD144 for a detailed look at the design
        self.vsc7448.init_sgmii(&[
            0,  // DEV1G_0   | SERDES1G_1  | Cubby 0
            1,  // DEV1G_1   | SERDES1G_2  | Cubby 1
            2,  // DEV1G_2   | SERDES1G_3  | Cubby 2
            3,  // DEV1G_3   | SERDES1G_4  | Cubby 3
            4,  // DEV1G_4   | SERDES1G_5  | Cubby 4
            5,  // DEV1G_5   | SERDES1G_6  | Cubby 5
            6,  // DEV1G_6   | SERDES1G_7  | Cubby 6
            7,  // DEV1G_7   | SERDES1G_8  | Cubby 7
            8,  // DEV2G5_0  | SERDES6G_0  | Cubby 8
            9,  // DEV2G5_1  | SERDES6G_1  | Cubby 9
            10, // DEV2G5_2  | SERDES6G_2  | Cubby 10
            11, // DEV2G5_3  | SERDES6G_3  | Cubby 11
            12, // DEV2G5_4  | SERDES6G_4  | Cubby 12
            13, // DEV2G5_5  | SERDES6G_5  | Cubby 13
            14, // DEV2G5_6  | SERDES6G_6  | Cubby 14
            15, // DEV2G5_7  | SERDES6G_7  | Cubby 15
            16, // DEV2G5_8  | SERDES6G_8  | Cubby 16
            17, // DEV2G5_9  | SERDES6G_9  | Cubby 17
            18, // DEV2G5_10 | SERDES6G_10 | Cubby 18
            19, // DEV2G5_11 | SERDES6G_11 | Cubby 19
            20, // DEV2G5_12 | SERDES6G_12 | Cubby 20
            21, // DEV2G5_13 | SERDES6G_13 | Cubby 21
            24, // DEV2G5_16 | SERDES6G_16 | Cubby 22
            25, // DEV2G5_17 | SERDES6G_17 | Cubby 23
            26, // DEV2G5_18 | SERDES6G_18 | Cubby 24
            27, // DEV2G5_19 | SERDES6G_19 | Cubby 25
            28, // DEV2G5_20 | SERDES6G_20 | Cubby 26
            29, // DEV2G5_21 | SERDES6G_21 | Cubby 27
            30, // DEV2G5_22 | SERDES6G_22 | Cubby 28
            31, // DEV2G5_23 | SERDES6G_23 | Cubby 29
            48, // Local SP
        ])?;
        self.vsc7448.init_10g_sgmii(&[
            51, // DEV2G5_27 | SERDES10G_2 | Cubby 30   (shadows DEV10G_2)
            52, // DEV2G5_28 | SERDES10G_3 | Cubby 31   (shadows DEV10G_3)
        ])?;

        self.phy_init(&sys)?;
        self.vsc7448.init_qsgmii(&[
            // Going to an on-board VSC8504 PHY (PHY4, U40), which is
            // configured over MIIM by the SP.
            //
            // 40 | DEV1G_16 | SERDES6G_14 | Peer SP
            // 41 | DEV1G_17 | SERDES6G_14 | PSC0
            // 42 | DEV1G_18 | SERDES6G_14 | PSC1
            // 43 | Unused
            40,
            // Going out to the front panel board, where there's a waiting
            // PHY that is configured by the FPGA.
            //
            // 44 | DEV1G_16 | SERDES6G_15 | Technician 0
            // 45 | DEV1G_17 | SERDES6G_15 | Technician 1
            // 42 | Unused
            // 43 | Unused
            44,
        ])?;

        self.vsc7448.init_sfi(&[
            49, //  DEV10G_0 | SERDES10G_0 | Tofino 2
        ])?;

        Ok(())
    }

    fn phy_init(&mut self, sys: &Sys) -> Result<(), VscError> {
        // Let's configure the on-board PHY first
        // Relevant pins are
        // - MIIM_SP_TO_PHY_MDC_2V5 (PC1)
        // - MIIM_SP_TO_PHY_MDIO_2V5 (PA2)
        // - MIIM_SP_TO_PHY_MDINT_2V5_L
        // - SP_TO_PHY4_COMA_MODE (PI10, internal pull-up)
        // - SP_TO_PHY4_RESET_L (PI9)
        //
        // The PHY talks on MIIM addresses 0x4-0x7 (configured by resistors
        // on the board)

        // TODO: wait for PLL lock to happen here
        use sys_api::*;

        let coma_mode = Port::I.pin(10);
        sys.gpio_set(coma_mode).unwrap();
        sys.gpio_configure_output(
            coma_mode,
            OutputType::PushPull,
            Speed::Low,
            Pull::None,
        )
        .unwrap();
        sys.gpio_reset(coma_mode).unwrap();

        // Make NRST low then switch it to output mode
        let nrst = Port::I.pin(9);
        sys.gpio_reset(nrst).unwrap();
        sys.gpio_configure_output(
            nrst,
            OutputType::PushPull,
            Speed::Low,
            Pull::None,
        )
        .unwrap();

        // Jiggle reset line, then wait 120 ms
        // SP_TO_LDO_PHY4_EN (PI6)
        let phy4_pwr_en = Port::I.pin(6);
        sys.gpio_reset(phy4_pwr_en).unwrap();
        sys.gpio_configure_output(
            phy4_pwr_en,
            OutputType::PushPull,
            Speed::Low,
            Pull::None,
        )
        .unwrap();
        sys.gpio_reset(phy4_pwr_en).unwrap();
        sleep_for(10);

        // Power on!
        sys.gpio_set(phy4_pwr_en).unwrap();
        sleep_for(4);
        // TODO: sleep for PG lines going high here

        sys.gpio_set(nrst).unwrap();
        sleep_for(120); // Wait for the chip to come out of reset

        // Initialize the PHY, then disable COMA_MODE
        init_vsc8504_phy(&mut Phy { port: 4, rw: self })?;
        sys.gpio_reset(coma_mode).unwrap();

        Ok(())
    }

    pub fn run(&mut self) -> ! {
        loop {
            for port in 4..8 {
                let mut vsc8504 = Phy { port, rw: self };
                let status: u16 =
                    vsc8504.read(phy::STANDARD::MODE_STATUS()).unwrap().into();
                ringbuf_entry!(Trace::Vsc8504StatusLink { port, status });

                // 100BASE-TX/FX Status Extension register
                let addr: PhyRegisterAddress<u16> =
                    PhyRegisterAddress::from_page_and_addr_unchecked(0, 16);
                let status: u16 = vsc8504.read(addr).unwrap();
                ringbuf_entry!(Trace::Vsc8504Status100Base { port, status });
            }
            sleep_for(100);
        }
    }
}
