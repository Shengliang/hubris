// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Server for managing the Gimlet sequencing process.

#![no_std]
#![no_main]

mod seq_spi;

use ringbuf::*;
use userlib::*;

use drv_gimlet_hf_api as hf_api;
use drv_gimlet_seq_api::{PowerState, SeqError};
use drv_i2c_api::{I2cDevice, ResponseCode};
use drv_ice40_spi_program as ice40;
use drv_spi_api as spi_api;
use drv_stm32xx_sys_api as sys_api;
use idol_runtime::RequestError;
use seq_spi::{Addr, Reg};

task_slot!(SYS, sys);
task_slot!(SPI, spi_driver);
task_slot!(I2C, i2c_driver);
task_slot!(HF, hf);

include!(concat!(env!("OUT_DIR"), "/i2c_config.rs"));

mod payload;

#[derive(Copy, Clone, PartialEq)]
enum Trace {
    Ice40Rails(bool, bool),
    Reprogram(bool),
    Programmed,
    Programming,
    Ice40PowerGoodV1P2(bool),
    Ice40PowerGoodV3P3(bool),
    RailsOff,
    Ident(u32),
    A1Status(u8),
    A2,
    A1Power(u8, u8),
    A0Power(u8),
    RailsOn,
    Done,
    GetState,
    SetState(PowerState, PowerState),
    LoadClockConfig,
    ClockConfigWrite(usize),
    ClockConfigSuccess(usize),
    ClockConfigFailed(usize, ResponseCode),
    None,
}

ringbuf!(Trace, 64, Trace::None);

#[export_name = "main"]
fn main() -> ! {
    let spi = spi_api::Spi::from(SPI.get_task_id());
    let sys = sys_api::Sys::from(SYS.get_task_id());

    // To allow for the possibility that we are restarting, rather than
    // starting, we take care during early sequencing to _not turn anything
    // off,_ only on. This means if it was _already_ on, the outputs should not
    // glitch.

    // Unconditionally set our power-good detects as inputs.
    //
    // This is the expected reset state, but, good to be sure.
    sys.gpio_configure(
        PGS_PORT,
        PG_V1P2_MASK | PG_V3P3_MASK,
        sys_api::Mode::Input,
        sys_api::OutputType::PushPull, // doesn't matter
        sys_api::Speed::High,
        PGS_PULL,
        sys_api::Alternate::AF0, // doesn't matter
    )
    .unwrap();

    // Unconditionally set our sequencing-related GPIOs to outputs.
    //
    // If the processor has reset, these will start out low. Since neither rail
    // has external pullups, this puts the regulators into a well-defined "off"
    // state instead of leaving them floating, which is the state when A2 power
    // starts coming up.
    //
    // If it's just our driver that has reset, this will have no effect, and
    // will continue driving the lines at whatever level we left them in.
    sys.gpio_configure(
        ENABLES_PORT,
        ENABLE_V1P2_MASK | ENABLE_V3P3_MASK,
        sys_api::Mode::Output,
        sys_api::OutputType::PushPull,
        sys_api::Speed::High,
        sys_api::Pull::None,
        sys_api::Alternate::AF0, // doesn't matter
    )
    .unwrap();

    // To talk to the sequencer we need to configure its pins, obvs. Note that
    // the SPI and CS lines are separately managed by the SPI server; the ice40
    // crate handles the CRESETB and CDONE signals, and takes care not to
    // generate surprise resets.
    ice40::configure_pins(&sys, &ICE40_CONFIG);

    let pg = sys.gpio_read_input(PGS_PORT).unwrap();
    let v1p2 = pg & PG_V1P2_MASK != 0;
    let v3p3 = pg & PG_V3P3_MASK != 0;

    ringbuf_entry!(Trace::Ice40Rails(v1p2, v3p3));

    // Force iCE40 CRESETB low before turning power on. This is nice because it
    // prevents the iCE40 from racing us and deciding it should try to load from
    // Flash. TODO: this may cause trouble with hot restarts, test.
    sys.gpio_set_reset(
        ICE40_CONFIG.creset_port,
        0,
        ICE40_CONFIG.creset_pin_mask,
    )
    .unwrap();

    // Begin, or resume, the power supply sequencing process for the FPGA. We're
    // going to be reading back our enable line states to get the real state
    // being seen by the regulators, etc.

    // The V1P2 regulator comes up first. It may already be on from a past life
    // of ours. Ensuring that it's on by writing the pin is just as cheap as
    // sensing its current state, and less code than _conditionally_ writing the
    // pin, so:
    sys.gpio_set_reset(ENABLES_PORT, ENABLE_V1P2_MASK, 0)
        .unwrap();

    // We don't actually know how long ago the regulator turned on. Could have
    // been _just now_ (above) or may have already been on. We'll use the PG pin
    // to detect when it's stable. But -- the PG pin on the LT3072 is initially
    // high when you turn the regulator on, and then takes time to drop if
    // there's a problem. So, to ensure that there has been at least 1ms since
    // regulator-on, we will delay for 2.
    hl::sleep_for(2);

    // Now, monitor the PG pin.
    loop {
        // active high
        let pg = sys.gpio_read_input(PGS_PORT).unwrap() & PG_V1P2_MASK != 0;
        ringbuf_entry!(Trace::Ice40PowerGoodV1P2(pg));
        if pg {
            break;
        }

        // Do _not_ burn CPU constantly polling, it's rude. We could also set up
        // pin-change interrupts but we only do this once per power on, so it
        // seems like a lot of work.
        hl::sleep_for(2);
    }

    // We believe V1P2 is good. Now, for V3P3! Set it active (high).
    sys.gpio_set_reset(ENABLES_PORT, ENABLE_V3P3_MASK, 0)
        .unwrap();

    // Delay to be sure.
    hl::sleep_for(2);

    // Now, monitor the PG pin.
    loop {
        // active high
        let pg = sys.gpio_read_input(PGS_PORT).unwrap() & PG_V3P3_MASK != 0;
        ringbuf_entry!(Trace::Ice40PowerGoodV3P3(pg));
        if pg {
            break;
        }

        // Do _not_ burn CPU constantly polling, it's rude.
        hl::sleep_for(2);
    }

    // Now, V2P5 is chained off V3P3 and comes up on its own with no
    // synchronization. It takes about 500us in practice. We'll delay for 1ms,
    // plus give the iCE40 a good 10ms to come out of power-down.
    hl::sleep_for(1 + 10);

    // Sequencer FPGA power supply sequencing (meta-sequencing?) is complete.

    // Now, let's find out if we need to program the sequencer.

    if let Some(hacks) = FPGA_HACK_PINS {
        // Some boards require certain pins to be put in certain states before
        // we can perform SPI communication with the design (rather than the
        // programming port). If this is such a board, apply those changes:
        for &(port, pin_mask, is_high) in hacks {
            sys.gpio_set_reset(
                port,
                if is_high { pin_mask } else { 0 },
                if is_high { 0 } else { pin_mask },
            )
            .unwrap();

            sys.gpio_configure(
                port,
                pin_mask,
                sys_api::Mode::Output,
                sys_api::OutputType::PushPull,
                sys_api::Speed::High,
                sys_api::Pull::None,
                sys_api::Alternate::AF0, // doesn't matter
            )
            .unwrap();
        }
    }

    if let Some((port, pin_mask)) = GLOBAL_RESET {
        // Also configure our design reset net -- the signal that resets the
        // logic _inside_ the FPGA instead of the FPGA itself. We're assuming
        // push-pull because all our boards with reset nets are lacking pullups
        // right now. It's active low, so, set up the pin before exposing the
        // output to ensure we don't glitch.
        sys.gpio_set_reset(port, pin_mask, 0).unwrap();
        sys.gpio_configure(
            port,
            pin_mask,
            sys_api::Mode::Output,
            sys_api::OutputType::PushPull,
            sys_api::Speed::High,
            sys_api::Pull::None,
            sys_api::Alternate::AF0, // doesn't matter
        )
        .unwrap();
    }

    // If the sequencer is already loaded and operational, the design loaded
    // into it should be willing to talk to us over SPI, and should be able to
    // serve up a recognizable ident code.
    let seq = seq_spi::SequencerFpga::new(spi.device(SEQ_SPI_DEVICE));

    let reprogram = !seq.valid_ident();
    ringbuf_entry!(Trace::Reprogram(reprogram));

    // We only want to reset and reprogram the FPGA when absolutely required.
    if reprogram {
        if let Some((port, pin_mask)) = GLOBAL_RESET {
            // Assert the design reset signal (not the same as the FPGA
            // programming logic reset signal). We do this during reprogramming
            // to avoid weird races that make our brains hurt.
            sys.gpio_set_reset(port, 0, pin_mask).unwrap();
        }

        let mut laps = 0;

        // Reprogramming will continue until morale improves -- to a point.
        loop {
            let prog = spi.device(ICE40_SPI_DEVICE);
            ringbuf_entry!(Trace::Programming);
            match reprogram_fpga(&prog, &sys, &ICE40_CONFIG) {
                Ok(()) => {
                    // yay
                    break;
                }
                Err(_) => {
                    // Try and put state back to something reasonable.  We
                    // don't know if we're still locked, so ignore the
                    // complaint if we're not.
                    let _ = prog.release();

                    laps += 1;

                    if laps >= 3 {
                        panic!(
                            "could not reprogram FPGA after {} \
                            attempts; if CS has been reworked, \
                            look for \
                            \"ATTENTION REWORKED CS\" in app.toml",
                            laps
                        );
                    }
                }
            }
        }

        if let Some((port, pin_mask)) = GLOBAL_RESET {
            // Deassert design reset signal. We set the pin, as it's
            // active low.
            sys.gpio_set_reset(port, pin_mask, 0).unwrap();
        }
    }

    ringbuf_entry!(Trace::Programmed);

    vcore_soc_off();
    ringbuf_entry!(Trace::RailsOff);

    let ident = seq.read_ident().unwrap();
    ringbuf_entry!(Trace::Ident(ident));

    loop {
        let mut status = [0u8];

        seq.read_bytes(Addr::PWRCTRL, &mut status).unwrap();
        ringbuf_entry!(Trace::A1Status(status[0]));

        if status[0] == 0 {
            break;
        }

        hl::sleep_for(1);
    }

    ringbuf_entry!(Trace::A2);

    let mut buffer = [0; idl::INCOMING_SIZE];
    let mut server = ServerImpl {
        state: PowerState::A2,
        clockgen: i2c_config::devices::idt8a34003(I2C.get_task_id())[0],
        seq,
    };

    loop {
        ringbuf_entry!(Trace::Done);
        idol_runtime::dispatch(&mut buffer, &mut server);
    }
}

struct ServerImpl {
    state: PowerState,
    clockgen: I2cDevice,
    seq: seq_spi::SequencerFpga,
}

impl idl::InOrderSequencerImpl for ServerImpl {
    fn get_state(
        &mut self,
        _: &RecvMessage,
    ) -> Result<PowerState, RequestError<SeqError>> {
        ringbuf_entry!(Trace::GetState);
        Ok(self.state)
    }

    fn set_state(
        &mut self,
        _: &RecvMessage,
        state: PowerState,
    ) -> Result<(), RequestError<SeqError>> {
        ringbuf_entry!(Trace::SetState(self.state, state));

        match (self.state, state) {
            (PowerState::A2, PowerState::A0) => {
                //
                // First, set our mux state to be the HostCPU
                //
                let hf = hf_api::HostFlash::from(HF.get_task_id());

                if let Err(_) = hf.set_mux(hf_api::HfMuxState::HostCPU) {
                    return Err(SeqError::MuxToHostCPUFailed.into());
                }

                //
                // We are going to pass through A1 on the way to A0.
                //
                let a1a0 = Reg::PWRCTRL::A1PWREN | Reg::PWRCTRL::A0A_EN;
                self.seq.write_bytes(Addr::PWRCTRL, &[a1a0]).unwrap();

                loop {
                    let mut power = [0u8, 0u8];

                    self.seq.read_bytes(Addr::A1SMSTATUS, &mut power).unwrap();
                    ringbuf_entry!(Trace::A1Power(power[0], power[1]));

                    if power[1] == 0x7 {
                        break;
                    }

                    hl::sleep_for(1);
                }

                //
                // And power up!
                //
                vcore_soc_on();
                ringbuf_entry!(Trace::RailsOn);

                //
                // Now wait for the end of Group C.
                //
                loop {
                    let mut power = [0u8];

                    self.seq.read_bytes(Addr::A0SMSTATUS, &mut power).unwrap();
                    ringbuf_entry!(Trace::A0Power(power[0]));

                    if power[0] == 0xc {
                        break;
                    }

                    hl::sleep_for(1);
                }

                self.state = PowerState::A0;
                Ok(())
            }

            (PowerState::A0, PowerState::A2) => {
                let hf = hf_api::HostFlash::from(HF.get_task_id());
                let a1a0 = Reg::PWRCTRL::A0C_DIS;

                self.seq.write_bytes(Addr::PWRCTRL, &[a1a0]).unwrap();
                vcore_soc_off();

                if let Err(_) = hf.set_mux(hf_api::HfMuxState::SP) {
                    return Err(SeqError::MuxToSPFailed.into());
                }

                self.state = PowerState::A2;
                ringbuf_entry!(Trace::A2);
                Ok(())
            }

            _ => Err(RequestError::Runtime(SeqError::IllegalTransition)),
        }
    }

    fn fans_on(
        &mut self,
        _: &RecvMessage,
    ) -> Result<(), RequestError<SeqError>> {
        let on = Reg::EARLY_POWER_CTRL::FANPWREN;
        self.seq.set_bytes(Addr::EARLY_POWER_CTRL, &[on]).unwrap();
        Ok(())
    }

    fn fans_off(
        &mut self,
        _: &RecvMessage,
    ) -> Result<(), RequestError<SeqError>> {
        let off = Reg::EARLY_POWER_CTRL::FANPWREN;
        self.seq
            .clear_bytes(Addr::EARLY_POWER_CTRL, &[off])
            .unwrap();
        Ok(())
    }

    fn load_clock_config(
        &mut self,
        _: &RecvMessage,
    ) -> Result<(), RequestError<SeqError>> {
        ringbuf_entry!(Trace::LoadClockConfig);

        let mut packet = 0;

        payload::idt8a3xxxx_payload(|buf| {
            ringbuf_entry!(Trace::ClockConfigWrite(packet));
            match self.clockgen.write(buf) {
                Err(err) => {
                    ringbuf_entry!(Trace::ClockConfigFailed(packet, err));
                    Err(SeqError::ClockConfigFailed)
                }

                Ok(_) => {
                    ringbuf_entry!(Trace::ClockConfigSuccess(packet));
                    packet += 1;
                    Ok(())
                }
            }
        })?;

        Ok(())
    }
}

fn reprogram_fpga(
    spi: &spi_api::SpiDevice,
    sys: &sys_api::Sys,
    config: &ice40::Config,
) -> Result<(), ice40::Ice40Error> {
    ice40::begin_bitstream_load(&spi, &sys, &config)?;

    // We've got the bitstream in Flash, so we can technically just send it in
    // one transaction, but we'll want chunking later -- so let's make sure
    // chunking works.
    let mut bitstream = COMPRESSED_BITSTREAM;
    let mut decompressor = gnarle::Decompressor::default();
    let mut chunk = [0; 256];
    while !bitstream.is_empty() || !decompressor.is_idle() {
        let out =
            gnarle::decompress(&mut decompressor, &mut bitstream, &mut chunk);
        ice40::continue_bitstream_load(&spi, out)?;
    }

    ice40::finish_bitstream_load(&spi, &sys, &config)
}

static COMPRESSED_BITSTREAM: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/fpga.bin.rle"));

cfg_if::cfg_if! {
    if #[cfg(any(target_board = "gimlet-a", target_board = "gimlet-b"))] {
        const SEQ_SPI_DEVICE: u8 = 0;
        const ICE40_SPI_DEVICE: u8 = 1;

        const ICE40_CONFIG: ice40::Config = ice40::Config {
            // CRESET net is SEQ_TO_SP_CRESET_L and hits PD5.
            creset_port: sys_api::Port::D,
            creset_pin_mask: 1 << 5,
            // CDONE net is SEQ_TO_SP_CDONE_L and hits PB4.
            cdone_port: sys_api::Port::B,
            cdone_pin_mask: 1 << 4,
        };

        const GLOBAL_RESET: Option<(sys_api::Port, u16)> = Some((
            sys_api::Port::A,
            1 << 6,
        ));

        // gimlet-a needs to have a pin flipped to mux the iCE40 SPI flash out
        // of circuit to be able to program the FPGA, because we accidentally
        // share a CS net between Flash and the iCE40.
        //
        // (port, mask, high_flag)
        #[cfg(target_board = "gimlet-a")]
        const FPGA_HACK_PINS: Option<&[(sys_api::Port, u16, bool)]> = Some(&[
            // SEQ_TO_SEQ_MUX_SEL, pulled high, we drive it low
            (sys_api::Port::I, 1 << 8, false),
        ]);

        #[cfg(target_board = "gimlet-b")]
        const FPGA_HACK_PINS: Option<&[(sys_api::Port, u16, bool)]> = None;

        const ENABLES_PORT: sys_api::Port = sys_api::Port::A;
        const ENABLE_V1P2_MASK: u16 = 1 << 15;
        const ENABLE_V3P3_MASK: u16 = 1 << 4;

        const PGS_PORT: sys_api::Port = sys_api::Port::C;
        const PG_V1P2_MASK: u16 = 1 << 7;
        const PG_V3P3_MASK: u16 = 1 << 6;
        // Gimlet provides external pullups.
        const PGS_PULL: sys_api::Pull = sys_api::Pull::None;

        fn vcore_soc_off() {
            use drv_i2c_devices::raa229618::Raa229618;
            let i2c = I2C.get_task_id();

            let (device, rail) = i2c_config::pmbus::vdd_vcore(i2c);
            let mut vdd_vcore = Raa229618::new(&device, rail);

            let (device, rail) = i2c_config::pmbus::vddcr_soc(i2c);
            let mut vddcr_soc = Raa229618::new(&device, rail);

            vdd_vcore.turn_off().unwrap();
            vddcr_soc.turn_off().unwrap();
        }

        fn vcore_soc_on() {
            use drv_i2c_devices::raa229618::Raa229618;
            let i2c = I2C.get_task_id();

            let (device, rail) = i2c_config::pmbus::vdd_vcore(i2c);
            let mut vdd_vcore = Raa229618::new(&device, rail);

            let (device, rail) = i2c_config::pmbus::vddcr_soc(i2c);
            let mut vddcr_soc = Raa229618::new(&device, rail);

            vdd_vcore.turn_on().unwrap();
            vddcr_soc.turn_on().unwrap();
        }
    } else {
        compiler_error!("unsupported target board");
    }
}

mod idl {
    use super::{PowerState, SeqError};

    include!(concat!(env!("OUT_DIR"), "/server_stub.rs"));
}
