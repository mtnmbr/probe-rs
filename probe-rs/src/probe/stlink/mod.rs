//! ST-Link probe implementation.

mod constants;
mod tools;
mod usb_interface;

use crate::{
    MemoryInterface,
    architecture::arm::{
        ArmError, DapAccess, FullyQualifiedApAddress, Pins, SwoAccess, SwoConfig, SwoMode,
        ap::{
            AccessPortType,
            memory_ap::{MemoryAp, MemoryApType},
            v1::valid_access_ports,
        },
        communication_interface::{ArmDebugInterface, DapProbe, SwdSequence},
        dp::{DpAddress, DpRegisterAddress},
        memory::ArmMemoryInterface,
        sequences::ArmDebugSequence,
        valid_32bit_arm_address,
    },
    probe::{
        DebugProbe, DebugProbeError, DebugProbeInfo, DebugProbeSelector, Probe, ProbeError,
        ProbeFactory, WireProtocol,
    },
};

use scroll::{BE, LE, Pread, Pwrite};

use std::collections::BTreeSet;
use std::thread;
use std::{cmp::Ordering, sync::Arc, time::Duration};

use constants::{JTagFrequencyToDivider, Mode, Status, SwdFrequencyToDelayCount, commands};
use usb_interface::{StLinkUsb, StLinkUsbDevice, TIMEOUT};

/// Maximum length of 32 bit reads in bytes.
///
/// Length has been determined by experimenting with
/// a ST-Link v2.
const STLINK_MAX_READ_LEN: usize = 6144;

/// Maximum length of 32 bit writes in bytes.
/// The length is limited to the largest 16-bit value which
/// is also a multiple of 4.
const STLINK_MAX_WRITE_LEN: usize = 0xFFFC;

const DP_PORT: u16 = 0xFFFF;

/// A factory for creating [`StLink`] probes.
#[derive(Debug)]
pub struct StLinkFactory;

impl std::fmt::Display for StLinkFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ST-LINK")
    }
}

impl ProbeFactory for StLinkFactory {
    fn open(&self, selector: &DebugProbeSelector) -> Result<Box<dyn DebugProbe>, DebugProbeError> {
        tracing::debug!("Opening ST-Link: {selector:?}");
        let device = StLinkUsbDevice::new_from_selector(selector)?;
        let mut stlink = StLink {
            name: format!("ST-Link {}", &device.info.version_name),
            device,
            hw_version: 0,
            jtag_version: 0,
            protocol: WireProtocol::Swd,
            swd_speed_khz: 1_800,
            jtag_speed_khz: 1_120,
            swo_enabled: false,

            opened_aps: vec![],
        };

        stlink.init()?;

        Ok(Box::new(stlink))
    }

    fn list_probes(&self) -> Vec<DebugProbeInfo> {
        tools::list_stlink_devices()
    }
}

/// An ST-Link debugger and programmer.
#[derive(Debug)]
pub struct StLink<D: StLinkUsb> {
    device: D,
    name: String,
    hw_version: u8,
    jtag_version: u8,
    protocol: WireProtocol,
    swd_speed_khz: u32,
    jtag_speed_khz: u32,
    swo_enabled: bool,

    /// List of opened APs
    opened_aps: Vec<u8>,
}

impl DebugProbe for StLink<StLinkUsbDevice> {
    fn get_name(&self) -> &str {
        &self.name
    }

    fn speed_khz(&self) -> u32 {
        match self.protocol {
            WireProtocol::Swd => self.swd_speed_khz,
            WireProtocol::Jtag => self.jtag_speed_khz,
        }
    }

    fn set_speed(&mut self, speed_khz: u32) -> Result<u32, DebugProbeError> {
        match self.hw_version.cmp(&3) {
            Ordering::Less => match self.protocol {
                WireProtocol::Swd => {
                    let actual_speed = SwdFrequencyToDelayCount::find_setting(speed_khz);

                    if let Some(actual_speed) = actual_speed {
                        self.set_swd_frequency(actual_speed)?;

                        self.swd_speed_khz = actual_speed.to_khz();

                        Ok(actual_speed.to_khz())
                    } else {
                        Err(DebugProbeError::UnsupportedSpeed(speed_khz))
                    }
                }
                WireProtocol::Jtag => {
                    let actual_speed = JTagFrequencyToDivider::find_setting(speed_khz);

                    if let Some(actual_speed) = actual_speed {
                        self.set_jtag_frequency(actual_speed)?;

                        self.jtag_speed_khz = actual_speed.to_khz();

                        Ok(actual_speed.to_khz())
                    } else {
                        Err(DebugProbeError::UnsupportedSpeed(speed_khz))
                    }
                }
            },
            Ordering::Equal | Ordering::Greater => {
                let (available, _) = self.get_communication_frequencies(self.protocol)?;

                let actual_speed_khz = available
                    .into_iter()
                    .filter(|speed| *speed <= speed_khz)
                    .max()
                    .ok_or(DebugProbeError::UnsupportedSpeed(speed_khz))?;

                self.set_communication_frequency(self.protocol, actual_speed_khz)?;

                match self.protocol {
                    WireProtocol::Swd => self.swd_speed_khz = actual_speed_khz,
                    WireProtocol::Jtag => self.jtag_speed_khz = actual_speed_khz,
                }

                Ok(actual_speed_khz)
            }
        }
    }

    #[tracing::instrument(skip(self))]
    fn attach(&mut self) -> Result<(), DebugProbeError> {
        self.enter_idle()?;

        let param = match self.protocol {
            WireProtocol::Jtag => {
                tracing::debug!("Switching protocol to JTAG");
                commands::JTAG_ENTER_JTAG_NO_CORE_RESET
            }
            WireProtocol::Swd => {
                tracing::debug!("Switching protocol to SWD");
                commands::JTAG_ENTER_SWD
            }
        };

        // Check and report the target voltage.
        let target_voltage = self
            .get_target_voltage()?
            .expect("The ST-Link returned None when it should only be able to return Some(f32) or an error. Please report this bug!");
        if target_voltage < crate::probe::LOW_TARGET_VOLTAGE_WARNING_THRESHOLD {
            tracing::warn!(
                "Target voltage (VAPP) is {:2.2} V. Is your target device powered?",
                target_voltage
            );
        } else {
            tracing::info!("Target voltage (VAPP): {:2.2} V", target_voltage);
        }

        let mut buf = [0; 2];
        self.send_jtag_command(
            &[commands::JTAG_COMMAND, commands::JTAG_ENTER2, param, 0],
            &[],
            &mut buf,
            TIMEOUT,
        )?;

        tracing::debug!("Successfully initialized {}.", self.protocol);

        // If the speed is not manually set, the probe will
        // use whatever speed has been configured before.
        //
        // To ensure the default speed is used if not changed,
        // we set the speed again here.
        match self.protocol {
            WireProtocol::Jtag => {
                self.set_speed(self.jtag_speed_khz)?;
            }
            WireProtocol::Swd => {
                self.set_speed(self.swd_speed_khz)?;
            }
        }

        Ok(())
    }

    fn detach(&mut self) -> Result<(), crate::Error> {
        tracing::debug!("Detaching from STLink.");
        if self.swo_enabled {
            self.disable_swo().map_err(crate::Error::Arm)?;
        }
        self.enter_idle()
            .map_err(|e| DebugProbeError::from(e).into())
    }

    fn target_reset(&mut self) -> Result<(), DebugProbeError> {
        let mut buf = [0; 2];
        self.send_jtag_command(
            &[
                commands::JTAG_COMMAND,
                commands::JTAG_DRIVE_NRST,
                commands::JTAG_DRIVE_NRST_PULSE,
            ],
            &[],
            &mut buf,
            TIMEOUT,
        )?;

        Ok(())
    }

    fn target_reset_assert(&mut self) -> Result<(), DebugProbeError> {
        let mut buf = [0; 2];
        self.send_jtag_command(
            &[
                commands::JTAG_COMMAND,
                commands::JTAG_DRIVE_NRST,
                commands::JTAG_DRIVE_NRST_LOW,
            ],
            &[],
            &mut buf,
            TIMEOUT,
        )?;

        Ok(())
    }

    fn target_reset_deassert(&mut self) -> Result<(), DebugProbeError> {
        let mut buf = [0; 2];
        self.send_jtag_command(
            &[
                commands::JTAG_COMMAND,
                commands::JTAG_DRIVE_NRST,
                commands::JTAG_DRIVE_NRST_HIGH,
            ],
            &[],
            &mut buf,
            TIMEOUT,
        )?;

        Ok(())
    }

    fn select_protocol(&mut self, protocol: WireProtocol) -> Result<(), DebugProbeError> {
        match protocol {
            WireProtocol::Jtag => self.protocol = WireProtocol::Jtag,
            WireProtocol::Swd => self.protocol = WireProtocol::Swd,
        }
        Ok(())
    }

    fn active_protocol(&self) -> Option<WireProtocol> {
        Some(self.protocol)
    }

    fn get_swo_interface(&self) -> Option<&dyn SwoAccess> {
        Some(self as _)
    }

    fn get_swo_interface_mut(&mut self) -> Option<&mut dyn SwoAccess> {
        Some(self as _)
    }

    fn has_arm_interface(&self) -> bool {
        true
    }

    fn into_probe(self: Box<Self>) -> Box<dyn DebugProbe> {
        self
    }

    fn try_get_arm_debug_interface<'probe>(
        self: Box<Self>,
        _sequence: Arc<dyn ArmDebugSequence>,
    ) -> Result<Box<dyn ArmDebugInterface + 'probe>, (Box<dyn DebugProbe>, ArmError)> {
        let interface = StlinkArmDebug::new(self);

        Ok(Box::new(interface))
    }

    fn get_target_voltage(&mut self) -> Result<Option<f32>, DebugProbeError> {
        let mut buf = [0; 8];
        self.device
            .write(&[commands::GET_TARGET_VOLTAGE], &[], &mut buf, TIMEOUT)
            .and_then(|_| {
                // The next two unwraps are safe!
                let a0 = buf[0..4].pread_with::<u32>(0, LE).unwrap();
                let a1 = buf[4..8].pread_with::<u32>(0, LE).unwrap();
                if a0 != 0 {
                    Ok(Some(2. * (a1 as f32) * 1.2 / (a0 as f32)))
                } else {
                    // Should never happen
                    Err(StlinkError::VoltageDivisionByZero)
                }
            })
            .map_err(|e| e.into())
    }
}

impl<D: StLinkUsb> Drop for StLink<D> {
    fn drop(&mut self) {
        // We ignore the error cases as we can't do much about it anyways.
        if self.swo_enabled {
            let _ = self.disable_swo();
        }
        let _ = self.enter_idle();
    }
}

impl StLink<StLinkUsbDevice> {
    fn swj_pins(
        &mut self,
        pin_out: u32,
        pin_select: u32,
        pin_wait: u32,
    ) -> Result<u32, DebugProbeError> {
        let mut nreset = Pins(0);
        nreset.set_nreset(true);
        let nreset_mask = nreset.0 as u32;

        // If only the reset pin is selected we perform the reset.
        // If something else is selected return an error as this is not supported on ST-Links.
        if pin_select == nreset_mask {
            if Pins(pin_out as u8).nreset() {
                self.target_reset_deassert()?;
            } else {
                self.target_reset_assert()?;
            }

            // Normally this would be the timeout we pass to the probe to settle the pins.
            // The ST-Link is not capable of this, so we just wait for this time on the host
            // and assume it has settled until then.
            thread::sleep(Duration::from_micros(pin_wait as u64));

            // We signal that we cannot read the pin state.
            Ok(0xFFFF_FFFF)
        } else {
            // This is not supported for ST-Links, unfortunately.
            Err(DebugProbeError::CommandNotSupportedByProbe {
                command_name: "swj_pins",
            })
        }
    }
}

impl<D: StLinkUsb> StLink<D> {
    /// Maximum number of bytes to send or receive for 32- and 16- bit transfers.
    ///
    /// 8-bit transfers have a maximum size of the maximum USB packet size (64 bytes for full speed).
    const _MAXIMUM_TRANSFER_SIZE: u32 = 1024;

    /// Minimum required STLink firmware version.
    const MIN_JTAG_VERSION: u8 = 26;

    /// Minimum required STLink V3 firmware version.
    ///
    /// Version 2 of the firmware (V3J2M1) has problems switching communication protocols.
    const MIN_JTAG_VERSION_V3: u8 = 3;

    /// Firmware version that adds multiple AP support.
    const MIN_JTAG_VERSION_MULTI_AP: u8 = 28;

    /// Firmware version which supports banked DP registers.
    ///
    /// This only applies to HW version 2, for version 3 we only support
    /// FW versions where this is supported.
    const MIN_JTAG_VERSION_DP_BANK_SEL: u8 = 32;

    /// Get the current mode of the ST-Link
    fn get_current_mode(&mut self) -> Result<Mode, StlinkError> {
        tracing::trace!("Getting current mode of device...");
        let mut buf = [0; 2];
        self.device
            .write(&[commands::GET_CURRENT_MODE], &[], &mut buf, TIMEOUT)?;

        let mode = match buf[0] {
            0 => Mode::Dfu,
            1 => Mode::MassStorage,
            2 => Mode::Jtag,
            3 => Mode::Swim,
            _ => return Err(StlinkError::UnknownMode),
        };

        tracing::debug!("Current device mode: {:?}", mode);

        Ok(mode)
    }

    /// Check if selecting different banks in the DP is supported.
    ///
    /// If this is not supported, some DP registers cannot be accessed.
    fn supports_dp_bank_selection(&self) -> bool {
        (self.hw_version == 2 && self.jtag_version >= Self::MIN_JTAG_VERSION_DP_BANK_SEL)
            || self.hw_version >= 3
    }

    /// Commands the ST-Link to enter idle mode.
    /// Internal helper.
    fn enter_idle(&mut self) -> Result<(), StlinkError> {
        let mode = self.get_current_mode()?;

        match mode {
            Mode::Jtag => self.device.write(
                &[commands::JTAG_COMMAND, commands::JTAG_EXIT],
                &[],
                &mut [],
                TIMEOUT,
            ),
            Mode::Dfu => self.device.write(
                &[commands::DFU_COMMAND, commands::DFU_EXIT],
                &[],
                &mut [],
                TIMEOUT,
            ),
            Mode::Swim => self.device.write(
                &[commands::SWIM_COMMAND, commands::SWIM_EXIT],
                &[],
                &mut [],
                TIMEOUT,
            ),
            _ => Ok(()),
        }
    }

    /// Reads the ST-Link's version.
    /// Returns a tuple (hardware version, firmware version).
    /// This method stores the version data on the struct to make later use of it.
    fn get_version(&mut self) -> Result<(u8, u8), StlinkError> {
        const HW_VERSION_SHIFT: u8 = 12;
        const HW_VERSION_MASK: u8 = 0x0F;
        const JTAG_VERSION_SHIFT: u8 = 6;
        const JTAG_VERSION_MASK: u8 = 0x3F;
        // GET_VERSION response structure:
        //   Byte 0-1:
        //     [15:12] Major/HW version
        //     [11:6]  JTAG/SWD version
        //     [5:0]   SWIM or MSC version
        //   Byte 2-3: ST_VID
        //   Byte 4-5: STLINK_PID
        let mut buf = [0; 6];
        self.device
            .write(&[commands::GET_VERSION], &[], &mut buf, TIMEOUT)
            .map(|_| {
                let version: u16 = buf[0..2].pread_with(0, BE).unwrap();
                self.hw_version = (version >> HW_VERSION_SHIFT) as u8 & HW_VERSION_MASK;
                self.jtag_version = (version >> JTAG_VERSION_SHIFT) as u8 & JTAG_VERSION_MASK;
            })?;

        // For the STLinkV3 we must use the extended get version command.
        if self.hw_version >= 3 {
            // GET_VERSION_EXT response structure (byte offsets)
            //  0: HW version
            //  1: SWIM version
            //  2: JTAG/SWD version
            //  3: MSC/VCP version
            //  4: Bridge version
            //  5: Power version
            //  6-7: reserved
            //  8-9: ST_VID
            //  10-11: STLINK_PID
            let mut buf = [0; 12];
            self.device
                .write(&[commands::GET_VERSION_EXT], &[], &mut buf, TIMEOUT)
                .map(|_| {
                    let version: u8 = buf[2..3].pread_with(0, LE).unwrap();
                    self.jtag_version = version;
                })?;
        }

        // Make sure everything is okay with the firmware we use.
        if self.jtag_version == 0 {
            Err(StlinkError::JTAGNotSupportedOnProbe)
        } else if self.hw_version < 3 && self.jtag_version < Self::MIN_JTAG_VERSION {
            Err(StlinkError::ProbeFirmwareOutdated(Self::MIN_JTAG_VERSION))
        } else if self.hw_version == 3 && self.jtag_version < Self::MIN_JTAG_VERSION_V3 {
            Err(StlinkError::ProbeFirmwareOutdated(
                Self::MIN_JTAG_VERSION_V3,
            ))
        } else {
            Ok((self.hw_version, self.jtag_version))
        }
    }

    /// Opens the ST-Link USB device and tries to identify the ST-Links version and its target voltage.
    /// Internal helper.
    fn init(&mut self) -> Result<(), StlinkError> {
        tracing::debug!("Initializing STLink...");

        if let Err(e) = self.enter_idle() {
            match e {
                StlinkError::Usb(_) => {
                    // Reset the device, and try to enter idle mode again
                    self.device.reset()?;

                    self.enter_idle()?;
                }
                // Other error occurred, return it
                _ => return Err(e),
            }
        }

        let version = self.get_version()?;
        tracing::debug!("STLink version: {:?}", version);

        if self.hw_version >= 3 {
            let (_, current) = self.get_communication_frequencies(WireProtocol::Swd)?;
            self.swd_speed_khz = current;

            let (_, current) = self.get_communication_frequencies(WireProtocol::Jtag)?;
            self.jtag_speed_khz = current;
        }

        Ok(())
    }

    /// sets the SWD frequency.
    pub fn set_swd_frequency(
        &mut self,
        frequency: SwdFrequencyToDelayCount,
    ) -> Result<(), DebugProbeError> {
        let mut buf = [0; 2];
        self.send_jtag_command(
            &[
                commands::JTAG_COMMAND,
                commands::SWD_SET_FREQ,
                frequency as u8,
            ],
            &[],
            &mut buf,
            TIMEOUT,
        )?;

        Ok(())
    }

    /// Sets the JTAG frequency.
    pub fn set_jtag_frequency(
        &mut self,
        frequency: JTagFrequencyToDivider,
    ) -> Result<(), DebugProbeError> {
        let mut buf = [0; 2];
        self.send_jtag_command(
            &[
                commands::JTAG_COMMAND,
                commands::JTAG_SET_FREQ,
                frequency as u8,
            ],
            &[],
            &mut buf,
            TIMEOUT,
        )?;

        Ok(())
    }

    /// Sets the communication frequency (V3 only)
    fn set_communication_frequency(
        &mut self,
        protocol: WireProtocol,
        frequency_khz: u32,
    ) -> Result<(), DebugProbeError> {
        if self.hw_version < 3 {
            return Err(DebugProbeError::CommandNotSupportedByProbe {
                command_name: "set_communication_frequency",
            });
        }

        let cmd_proto = match protocol {
            WireProtocol::Swd => 0,
            WireProtocol::Jtag => 1,
        };

        let mut command = vec![commands::JTAG_COMMAND, commands::SET_COM_FREQ, cmd_proto, 0];
        command.extend_from_slice(&frequency_khz.to_le_bytes());

        let mut buf = [0; 8];
        self.send_jtag_command(&command, &[], &mut buf, TIMEOUT)?;

        Ok(())
    }

    /// Returns the current and available communication frequencies (V3 only)
    fn get_communication_frequencies(
        &mut self,
        protocol: WireProtocol,
    ) -> Result<(Vec<u32>, u32), StlinkError> {
        let cmd_proto = match protocol {
            WireProtocol::Swd => 0,
            WireProtocol::Jtag => 1,
        };

        let mut buf = [0; 52];
        self.send_jtag_command(
            &[commands::JTAG_COMMAND, commands::GET_COM_FREQ, cmd_proto],
            &[],
            &mut buf,
            TIMEOUT,
        )?;

        let mut values = buf
            .chunks(4)
            .map(|chunk| chunk.pread_with::<u32>(0, LE).unwrap())
            .collect::<Vec<u32>>();

        let current = values[1];
        let n = std::cmp::min(values[2], 10) as usize;

        values.rotate_left(3);
        values.truncate(n);

        Ok((values, current))
    }

    /// Select an AP to use
    ///
    /// On newer ST-Links (JTAG Version >= 28), multiple APs are supported.
    /// To switch between APs, dedicated commands have to be used. For older
    /// ST-Links, we can only use AP 0. If an AP other than 0 is used on these
    /// probes, an error is returned.
    fn select_ap(&mut self, ap: u8) -> Result<(), DebugProbeError> {
        // Check if we can use APs other an AP 0.
        // Older versions of the ST-Link software don't support this.
        if self.hw_version < 3 && self.jtag_version < Self::MIN_JTAG_VERSION_MULTI_AP {
            if ap != 0 {
                return Err(
                    StlinkError::ProbeFirmwareOutdated(Self::MIN_JTAG_VERSION_MULTI_AP).into(),
                );
            }
        } else if !self.opened_aps.contains(&ap) {
            tracing::debug!("Opening AP {}", ap);
            self.open_ap(ap)?;
            self.opened_aps.push(ap);
        } else {
            tracing::trace!("AP {} already open.", ap);
        }

        Ok(())
    }

    /// Open a specific AP, which will be used for all future commands.
    ///
    /// This is only supported on ST-Link V3, or older ST-Links with
    /// a JTAG version >= `MIN_JTAG_VERSION_MULTI_AP`.
    fn open_ap(&mut self, apsel: u8) -> Result<(), DebugProbeError> {
        // Ensure this command is actually supported
        if self.hw_version < 3 && self.jtag_version < Self::MIN_JTAG_VERSION_MULTI_AP {
            return Err(DebugProbeError::CommandNotSupportedByProbe {
                command_name: "open_ap",
            });
        }

        let mut buf = [0; 2];
        tracing::trace!("JTAG_INIT_AP {}", apsel);
        retry_on_wait(|| {
            self.send_jtag_command(
                &[commands::JTAG_COMMAND, commands::JTAG_INIT_AP, apsel],
                &[],
                &mut buf,
                TIMEOUT,
            )
        })?;

        Ok(())
    }

    /// Close a specific AP, which was opened with `open_ap`.
    ///
    /// This is only supported on ST-Link V3, or older ST-Links with
    /// a JTAG version >= `MIN_JTAG_VERSION_MULTI_AP`.
    fn _close_ap(&mut self, apsel: u8) -> Result<(), DebugProbeError> {
        // Ensure this command is actually supported
        if self.hw_version < 3 && self.jtag_version < Self::MIN_JTAG_VERSION_MULTI_AP {
            return Err(DebugProbeError::CommandNotSupportedByProbe {
                command_name: "close_ap",
            });
        }

        let mut buf = [0; 2];
        tracing::trace!("JTAG_CLOSE_AP {}", apsel);
        retry_on_wait(|| {
            self.send_jtag_command(
                &[commands::JTAG_COMMAND, commands::JTAG_CLOSE_AP_DBG, apsel],
                &[],
                &mut buf,
                TIMEOUT,
            )
        })?;

        Ok(())
    }

    fn send_jtag_command(
        &mut self,
        cmd: &[u8],
        write_data: &[u8],
        read_data: &mut [u8],
        timeout: Duration,
    ) -> Result<(), StlinkError> {
        self.device.write(cmd, write_data, read_data, timeout)?;
        match Status::from(read_data[0]) {
            Status::JtagOk => Ok(()),
            status => {
                tracing::warn!("send_jtag_command {} failed: {:?}", cmd[0], status);
                Err(StlinkError::CommandFailed(status))
            }
        }
    }

    /// Starts reading SWO trace data.
    pub fn start_trace_reception(&mut self, config: &SwoConfig) -> Result<(), DebugProbeError> {
        let mut buf = [0; 2];
        let bufsize = 4096u16.to_le_bytes();
        let baud = config.baud().to_le_bytes();
        let mut command = vec![commands::JTAG_COMMAND, commands::SWO_START_TRACE_RECEPTION];
        command.extend_from_slice(&bufsize);
        command.extend_from_slice(&baud);

        self.send_jtag_command(&command, &[], &mut buf, TIMEOUT)?;

        self.swo_enabled = true;

        Ok(())
    }

    /// Stops reading SWO trace data.
    pub fn stop_trace_reception(&mut self) -> Result<(), DebugProbeError> {
        let mut buf = [0; 2];

        self.send_jtag_command(
            &[commands::JTAG_COMMAND, commands::SWO_STOP_TRACE_RECEPTION],
            &[],
            &mut buf,
            TIMEOUT,
        )?;

        self.swo_enabled = false;

        Ok(())
    }

    /// Gets the SWO count from the ST-Link probe.
    fn read_swo_available_byte_count(&mut self) -> Result<usize, DebugProbeError> {
        let mut buf = [0; 2];
        self.device.write(
            &[
                commands::JTAG_COMMAND,
                commands::SWO_GET_TRACE_NEW_RECORD_NB,
            ],
            &[],
            &mut buf,
            TIMEOUT,
        )?;
        Ok(buf.pread::<u16>(0).unwrap() as usize)
    }

    /// Reads the actual data from the SWO buffer on the ST-Link.
    fn read_swo_data(&mut self, timeout: Duration) -> Result<Vec<u8>, DebugProbeError> {
        // The byte count always needs to be polled first, otherwise
        // the ST-Link won't return any data.
        let mut buf = vec![0; self.read_swo_available_byte_count()?];
        let bytes_read = self.device.read_swo(&mut buf, timeout)?;
        buf.truncate(bytes_read);
        Ok(buf)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    fn get_last_rw_status(&mut self) -> Result<(), StlinkError> {
        let mut receive_buffer = [0u8; 12];

        self.send_jtag_command(
            &[commands::JTAG_COMMAND, commands::JTAG_GETLASTRWSTATUS2],
            &[],
            &mut receive_buffer,
            TIMEOUT,
        )?;

        Ok(())
    }

    /// Reads the DAP register on the specified port and address.
    fn read_register(&mut self, port: u16, addr: u8) -> Result<u32, DebugProbeError> {
        let port = port.to_le_bytes();

        let cmd = &[
            commands::JTAG_COMMAND,
            commands::JTAG_READ_DAP_REG,
            port[0],
            port[1],
            addr,
            0, // Maximum address for DAP registers is 0xFC
        ];
        let mut buf = [0; 8];
        retry_on_wait(|| self.send_jtag_command(cmd, &[], &mut buf, TIMEOUT))?;
        // Unwrap is ok!
        Ok(buf[4..8].pread_with(0, LE).unwrap())
    }

    /// Writes a value to the DAP register on the specified port and address.
    fn write_register(&mut self, port: u16, addr: u8, value: u32) -> Result<(), DebugProbeError> {
        let port = port.to_le_bytes();
        let bytes = value.to_le_bytes();

        let cmd = &[
            commands::JTAG_COMMAND,
            commands::JTAG_WRITE_DAP_REG,
            port[0],
            port[1],
            addr,
            0, // Maximum address for DAP registers is 0xFC
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
        ];
        let mut buf = [0; 2];

        retry_on_wait(|| self.send_jtag_command(cmd, &[], &mut buf, TIMEOUT))?;

        Ok(())
    }

    // Limit log verbosity to "trace", to avoid spamming the log with read/write operations.
    #[tracing::instrument(level="trace", skip(self, data, apsel), fields(ap=apsel, length= data.len()))]
    fn read_mem_32bit(
        &mut self,
        address: u32,
        data: &mut [u8],
        apsel: u8,
    ) -> Result<(), DebugProbeError> {
        // Do not attempt to read if there is no data to read.
        if data.is_empty() {
            return Ok(());
        }

        self.select_ap(apsel)?;

        // Ensure maximum read length is not exceeded.
        assert!(
            data.len() <= STLINK_MAX_READ_LEN,
            "Maximum read length for STLink is {STLINK_MAX_READ_LEN} bytes"
        );

        assert!(
            data.len() % 4 == 0,
            "Data length has to be a multiple of 4 for 32 bit reads"
        );

        if address % 4 != 0 {
            return Err(DebugProbeError::from(StlinkError::UnalignedAddress));
        }

        retry_on_wait(|| {
            self.device.write(
                &memory_command(commands::JTAG_READMEM_32BIT, address, data.len(), apsel),
                &[],
                data,
                TIMEOUT,
            )?;

            self.get_last_rw_status()
        })?;

        tracing::trace!("Read ok");

        Ok(())
    }

    #[tracing::instrument(level="trace", skip(self, data, apsel), fields(ap=apsel, length= data.len()))]
    fn read_mem_16bit(
        &mut self,
        address: u32,
        data: &mut [u8],
        apsel: u8,
    ) -> Result<(), DebugProbeError> {
        // Do not attempt to read if there is no data to read.
        if data.is_empty() {
            return Ok(());
        }

        self.select_ap(apsel)?;

        // TODO what is the max length?

        assert!(
            data.len() % 2 == 0,
            "Data length has to be a multiple of 2 for 16 bit reads"
        );

        if address % 2 != 0 {
            return Err(DebugProbeError::from(StlinkError::UnalignedAddress));
        }

        retry_on_wait(|| {
            self.device.write(
                &memory_command(commands::JTAG_READMEM_16BIT, address, data.len(), apsel),
                &[],
                data,
                TIMEOUT,
            )?;

            self.get_last_rw_status()
        })?;

        tracing::trace!("Read ok");

        Ok(())
    }

    fn read_mem_8bit(
        &mut self,
        address: u32,
        length: u16,
        apsel: u8,
    ) -> Result<Vec<u8>, DebugProbeError> {
        // Do not attempt to read if there is no data to read.
        if length == 0 {
            return Ok(vec![]);
        }

        self.select_ap(apsel)?;

        tracing::trace!("read_mem_8bit");

        if self.hw_version < 3 {
            assert!(
                length <= 64,
                "8-Bit reads are limited to 64 bytes on ST-Link v2"
            );
        } else {
            // This 255 byte limitation was empirically derived by @disasm @diondokter and @Yatekii
            // on various STM32 chips and different ST-Linkv3 versions (J5, J7).
            // It works until 255. 256 and above fail. Apparently it *should* work with up to
            // 512 bytes but those tries were not fruitful.
            assert!(
                length <= 255,
                "8-Bit reads are limited to 255 bytes on ST-Link v3"
            );
        }

        // The receive buffer must be at least two bytes in size, otherwise
        // a USB overflow error occurs.
        let buffer_len = length.max(2) as usize;

        let mut receive_buffer = vec![0u8; buffer_len];

        tracing::trace!("Read mem 8 bit, address={:08x}, length={}", address, length);

        retry_on_wait(|| {
            self.device.write(
                &memory_command(commands::JTAG_READMEM_8BIT, address, length as usize, apsel),
                &[],
                &mut receive_buffer,
                TIMEOUT,
            )?;

            if length == 1 {
                receive_buffer.resize(length as usize, 0)
            }

            self.get_last_rw_status()
        })?;

        Ok(receive_buffer)
    }

    fn write_mem_32bit(
        &mut self,
        address: u32,
        data: &[u8],
        apsel: u8,
    ) -> Result<(), DebugProbeError> {
        // Do not attempt to write if there is no data.
        if data.is_empty() {
            return Ok(());
        }

        self.select_ap(apsel)?;

        tracing::trace!("write_mem_32bit");
        let length = data.len();

        // Maximum supported read length is 2^16 bytes.
        assert!(
            length <= STLINK_MAX_WRITE_LEN,
            "Maximum write length for STLink is {STLINK_MAX_WRITE_LEN} bytes"
        );

        assert!(
            data.len() % 4 == 0,
            "Data length has to be a multiple of 4 for 32 bit writes"
        );

        if address % 4 != 0 {
            return Err(DebugProbeError::from(StlinkError::UnalignedAddress));
        }

        retry_on_wait(|| {
            self.device.write(
                &memory_command(commands::JTAG_WRITEMEM_32BIT, address, data.len(), apsel),
                data,
                &mut [],
                TIMEOUT,
            )?;

            self.get_last_rw_status()
        })?;

        Ok(())
    }

    fn write_mem_16bit(
        &mut self,
        address: u32,
        data: &[u8],
        apsel: u8,
    ) -> Result<(), DebugProbeError> {
        // Do not attempt to write if there is no data.
        if data.is_empty() {
            return Ok(());
        }

        self.select_ap(apsel)?;

        tracing::trace!("write_mem_16bit");

        // TODO what is the maximum supported length?

        assert!(
            data.len() % 2 == 0,
            "Data length has to be a multiple of 2 for 16 bit writes"
        );

        if address % 2 != 0 {
            return Err(DebugProbeError::from(StlinkError::UnalignedAddress));
        }

        retry_on_wait(|| {
            self.device.write(
                &memory_command(commands::JTAG_WRITEMEM_16BIT, address, data.len(), apsel),
                data,
                &mut [],
                TIMEOUT,
            )?;

            self.get_last_rw_status()
        })?;

        Ok(())
    }

    fn write_mem_8bit(
        &mut self,
        address: u32,
        data: &[u8],
        apsel: u8,
    ) -> Result<(), DebugProbeError> {
        // Do not attempt to write if there is no data. Doing so would result in endless retry.
        if data.is_empty() {
            return Ok(());
        }

        self.select_ap(apsel)?;

        tracing::trace!("write_mem_8bit");
        let byte_length = data.len();

        if self.hw_version < 3 {
            assert!(
                byte_length <= 64,
                "8-Bit writes are limited to 64 bytes on ST-Link v2"
            );
        } else {
            assert!(
                byte_length <= 512,
                "8-Bit writes are limited to 512 bytes on ST-Link v3"
            );
        }

        retry_on_wait(|| {
            self.device.write(
                &memory_command(commands::JTAG_WRITEMEM_8BIT, address, data.len(), apsel),
                data,
                &mut [],
                TIMEOUT,
            )?;

            self.get_last_rw_status()
        })?;

        Ok(())
    }

    fn _read_debug_reg(&mut self, address: u32) -> Result<u32, DebugProbeError> {
        tracing::trace!("Read debug reg {:08x}", address);
        let mut buff = [0u8; 8];

        let addbytes = address.to_le_bytes();
        self.send_jtag_command(
            &[
                commands::JTAG_COMMAND,
                commands::JTAG_READ_DEBUG_REG,
                addbytes[0],
                addbytes[1],
                addbytes[2],
                addbytes[3],
            ],
            &[],
            &mut buff,
            TIMEOUT,
        )?;

        Ok(buff.pread(4).unwrap())
    }

    fn _write_debug_reg(&mut self, address: u32, value: u32) -> Result<(), DebugProbeError> {
        tracing::trace!("Write debug reg {:08x}", address);
        let mut buff = [0u8; 2];

        let mut cmd = [0u8; 2 + 4 + 4];
        cmd[0] = commands::JTAG_COMMAND;
        cmd[1] = commands::JTAG_WRITE_DEBUG_REG;

        cmd.pwrite_with(address, 2, LE).unwrap();
        cmd.pwrite_with(value, 6, LE).unwrap();

        self.send_jtag_command(&cmd, &[], &mut buff, TIMEOUT)?;

        Ok(())
    }
}

const fn memory_command(command: u8, address: u32, len: usize, apsel: u8) -> [u8; 9] {
    let addbytes = address.to_le_bytes();
    let data_length = len.to_le_bytes();
    [
        commands::JTAG_COMMAND,
        command,
        addbytes[0],
        addbytes[1],
        addbytes[2],
        addbytes[3],
        data_length[0],
        data_length[1],
        apsel,
    ]
}

impl<D: StLinkUsb> SwoAccess for StLink<D> {
    fn enable_swo(&mut self, config: &SwoConfig) -> Result<(), ArmError> {
        match config.mode() {
            SwoMode::Uart => {
                self.start_trace_reception(config)?;
                Ok(())
            }
            SwoMode::Manchester => Err(ArmError::Probe(
                StlinkError::ManchesterSwoNotSupported.into(),
            )),
        }
    }

    fn disable_swo(&mut self) -> Result<(), ArmError> {
        self.stop_trace_reception()?;
        Ok(())
    }

    fn read_swo_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>, ArmError> {
        let data = self.read_swo_data(timeout)?;
        Ok(data)
    }
}

/// ST-Link specific errors.
#[derive(thiserror::Error, Debug, docsplay::Display)]
pub enum StlinkError {
    /// Invalid voltage values returned by probe.
    VoltageDivisionByZero,

    /// Probe is in an unknown mode.
    UnknownMode,

    /// Current version of the STLink firmware does not support accessing banked DP registers.
    BanksNotAllowedOnDPRegister,

    /// Not enough bytes were written. Expected {should} but only {is} were written.
    NotEnoughBytesWritten {
        /// The number of bytes actually written
        is: usize,
        /// The number of bytes that should have been written
        should: usize,
    },

    /// USB endpoint not found.
    EndpointNotFound,

    /// Command failed with status {0:?}.
    CommandFailed(Status),

    /// The probe does not support JTAG.
    JTAGNotSupportedOnProbe,

    /// The probe does not support SWO with Manchester encoding.
    ManchesterSwoNotSupported,

    /// The probe does not support multidrop SWD.
    MultidropNotSupported,

    /// Attempted unaligned access.
    UnalignedAddress,

    /// The firmware on the probe is outdated, and not supported by probe-rs. The minimum supported firmware version is {0}.
    /// Use the ST-Link updater utility to update your probe firmware.
    ProbeFirmwareOutdated(u8),

    /// USB error.
    Usb(#[from] std::io::Error),
}

impl ProbeError for StlinkError {}

#[derive(Debug)]
struct StlinkArmDebug {
    probe: Box<StLink<StLinkUsbDevice>>,

    /// The ST-Link probes don't support SWD multidrop, so we always use the default DP.
    ///
    /// This flag tracks if we are connected to a DP.
    connected_to_dp: bool,

    /// Information about the APs of the target.
    /// APs are identified by a number, starting from zero.
    pub access_ports: BTreeSet<FullyQualifiedApAddress>,
}

impl StlinkArmDebug {
    fn new(probe: Box<StLink<StLinkUsbDevice>>) -> Self {
        // Determine the number and type of available APs.
        Self {
            probe,
            access_ports: BTreeSet::new(),
            connected_to_dp: false,
        }
    }

    fn select_dp(&mut self, dp: DpAddress) -> Result<(), ArmError> {
        if dp != DpAddress::Default {
            return Err(DebugProbeError::from(StlinkError::MultidropNotSupported).into());
        }

        if !self.connected_to_dp {
            // We don't need to explicitly select a DP when using the ST-Link,
            // so we only detect the connected APs here.
            //
            // It's however important that we set this flag here, so we don't end up recursively calling this function.
            self.connected_to_dp = true;

            // Determine the number and type of available APs.
            self.access_ports = valid_access_ports(self, DpAddress::Default)
                .into_iter()
                .collect();

            self.access_ports.iter().for_each(|addr| {
                tracing::debug!("AP {:#x?}", addr);
            });
        }

        Ok(())
    }

    fn select_dp_and_dp_bank(
        &mut self,
        dp: DpAddress,
        address: DpRegisterAddress,
    ) -> Result<(), ArmError> {
        self.select_dp(dp)?;

        let Some(bank) = address.bank else {
            return Ok(());
        };

        if bank != 0 && !self.probe.supports_dp_bank_selection() {
            tracing::warn!(
                "Trying to access DP register at address {address:#x?}, which is not supported on ST-Links."
            );
            return Err(DebugProbeError::from(StlinkError::BanksNotAllowedOnDPRegister).into());
        }

        Ok(())
    }

    fn select_ap_and_ap_bank(
        &mut self,
        ap: &FullyQualifiedApAddress,
        _address: u64,
    ) -> Result<(), ArmError> {
        self.select_dp(ap.dp())?;
        self.probe.select_ap(ap.ap_v1()?)?;

        Ok(())
    }
}

impl DapAccess for StlinkArmDebug {
    #[tracing::instrument(skip(self), fields(value))]
    fn read_raw_dp_register(
        &mut self,
        dp: DpAddress,
        address: DpRegisterAddress,
    ) -> Result<u32, ArmError> {
        self.select_dp_and_dp_bank(dp, address)?;
        let result = self.probe.read_register(DP_PORT, address.into())?;

        tracing::Span::current().record("value", result);

        tracing::debug!("Read succesful");

        Ok(result)
    }

    #[tracing::instrument(skip(self))]
    fn write_raw_dp_register(
        &mut self,
        dp: DpAddress,
        address: DpRegisterAddress,
        value: u32,
    ) -> Result<(), ArmError> {
        self.select_dp_and_dp_bank(dp, address)?;

        self.probe.write_register(DP_PORT, address.into(), value)?;
        Ok(())
    }

    fn read_raw_ap_register(
        &mut self,
        ap: &FullyQualifiedApAddress,
        address: u64,
    ) -> Result<u32, ArmError> {
        if ap.ap().is_v2() {
            return Err(ArmError::NotImplemented(
                "ST-Link does not yet support APv2",
            ));
        }
        self.select_ap_and_ap_bank(ap, address)?;

        let value = self
            .probe
            .read_register(ap.ap_v1()? as u16, (address & 0xFF) as u8)?;

        Ok(value)
    }

    fn write_raw_ap_register(
        &mut self,
        ap: &FullyQualifiedApAddress,
        address: u64,
        value: u32,
    ) -> Result<(), ArmError> {
        if ap.ap().is_v2() {
            return Err(ArmError::NotImplemented(
                "ST-Link does not yet support APv2",
            ));
        }
        self.select_ap_and_ap_bank(ap, address)?;

        self.probe
            .write_register(ap.ap_v1()? as u16, (address & 0xFF) as u8, value)?;

        Ok(())
    }

    fn try_dap_probe(&self) -> Option<&dyn DapProbe> {
        None
    }

    fn try_dap_probe_mut(&mut self) -> Option<&mut dyn DapProbe> {
        None
    }
}

impl ArmDebugInterface for StlinkArmDebug {
    fn memory_interface(
        &mut self,
        access_port: &FullyQualifiedApAddress,
    ) -> Result<Box<dyn ArmMemoryInterface + '_>, ArmError> {
        let mem_ap = MemoryAp::new(self, access_port)?;
        let interface = StLinkMemoryInterface {
            probe: self,
            current_ap: mem_ap,
        };

        Ok(Box::new(interface) as _)
    }

    fn access_ports(
        &mut self,
        dp: DpAddress,
    ) -> Result<BTreeSet<FullyQualifiedApAddress>, ArmError> {
        self.select_dp(dp)?;

        Ok(self.access_ports.clone())
    }

    fn close(self: Box<Self>) -> Probe {
        Probe::from_attached_probe(self.probe)
    }

    fn current_debug_port(&self) -> Option<DpAddress> {
        if self.connected_to_dp {
            // SWD multidrop is not supported on ST-Link
            Some(DpAddress::Default)
        } else {
            None
        }
    }

    fn select_debug_port(&mut self, dp: DpAddress) -> Result<(), ArmError> {
        self.select_dp(dp)
    }

    fn reinitialize(&mut self) -> Result<(), ArmError> {
        Ok(())
    }
}

impl SwdSequence for StlinkArmDebug {
    fn swj_sequence(&mut self, _bit_len: u8, _bits: u64) -> Result<(), DebugProbeError> {
        // This is not supported for ST-Links, unfortunately.
        Err(DebugProbeError::CommandNotSupportedByProbe {
            command_name: "swj_sequence",
        })
    }

    fn swj_pins(
        &mut self,
        pin_out: u32,
        pin_select: u32,
        pin_wait: u32,
    ) -> Result<u32, DebugProbeError> {
        self.probe.swj_pins(pin_out, pin_select, pin_wait)
    }
}

impl SwoAccess for StlinkArmDebug {
    fn enable_swo(&mut self, config: &SwoConfig) -> Result<(), ArmError> {
        self.probe.enable_swo(config)
    }

    fn disable_swo(&mut self) -> Result<(), ArmError> {
        self.probe.disable_swo()
    }

    fn read_swo_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>, ArmError> {
        self.probe.read_swo_timeout(timeout)
    }
}

#[derive(Debug)]
struct StLinkMemoryInterface<'probe> {
    probe: &'probe mut StlinkArmDebug,
    current_ap: MemoryAp,
}

impl SwdSequence for StLinkMemoryInterface<'_> {
    fn swj_sequence(&mut self, bit_len: u8, bits: u64) -> Result<(), DebugProbeError> {
        self.probe.swj_sequence(bit_len, bits)
    }

    fn swj_pins(
        &mut self,
        pin_out: u32,
        pin_select: u32,
        pin_wait: u32,
    ) -> Result<u32, DebugProbeError> {
        self.probe.swj_pins(pin_out, pin_select, pin_wait)
    }
}

impl MemoryInterface<ArmError> for StLinkMemoryInterface<'_> {
    fn supports_native_64bit_access(&mut self) -> bool {
        false
    }

    fn read_64(&mut self, address: u64, data: &mut [u64]) -> Result<(), ArmError> {
        let address = valid_32bit_arm_address(address)?;

        // ST-Link V3 requires the data phase to be non-empty. For empty data,
        // return success.
        if data.is_empty() {
            return Ok(());
        }

        for (i, d) in data.iter_mut().enumerate() {
            let mut buff = vec![0u8; 8];

            self.probe.probe.read_mem_32bit(
                address + (i * 8) as u32,
                &mut buff,
                self.current_ap.ap_address().ap_v1()?,
            )?;

            *d = u64::from_le_bytes(buff.try_into().unwrap());
        }

        Ok(())
    }

    fn read_32(&mut self, address: u64, data: &mut [u32]) -> Result<(), ArmError> {
        let address = valid_32bit_arm_address(address)?;

        // ST-Link V3 requires the data phase to be non-empty. For empty data,
        // return success.
        if data.is_empty() {
            return Ok(());
        }

        // Read needs to be chunked into chunks with appropiate max length (see STLINK_MAX_READ_LEN).
        for (index, chunk) in data.chunks_mut(STLINK_MAX_READ_LEN / 4).enumerate() {
            let mut buff = vec![0u8; 4 * chunk.len()];

            self.probe.probe.read_mem_32bit(
                address + (index * STLINK_MAX_READ_LEN) as u32,
                &mut buff,
                self.current_ap.ap_address().ap_v1()?,
            )?;

            for (index, word) in buff.chunks_exact(4).enumerate() {
                chunk[index] = u32::from_le_bytes(word.try_into().unwrap());
            }
        }

        Ok(())
    }

    fn read_16(&mut self, address: u64, data: &mut [u16]) -> Result<(), ArmError> {
        let address = valid_32bit_arm_address(address)?;

        // ST-Link V3 requires the data phase to be non-empty. For empty data,
        // return success.
        if data.is_empty() {
            return Ok(());
        }

        // Read needs to be chunked into chunks of appropriate max length of the probe
        // use half the limits of 8bit accesses to be conservative. TODO can we increase this?
        let chunk_size = if self.probe.probe.hw_version < 3 {
            32
        } else {
            64
        };

        for (index, chunk) in data.chunks_mut(chunk_size).enumerate() {
            let mut buff = vec![0u8; 2 * chunk.len()];
            self.probe.probe.read_mem_16bit(
                address + (index * chunk_size) as u32,
                &mut buff,
                self.current_ap.ap_address().ap_v1()?,
            )?;

            for (index, word) in buff.chunks_exact(2).enumerate() {
                chunk[index] = u16::from_le_bytes(word.try_into().unwrap());
            }
        }

        Ok(())
    }

    fn read_8(&mut self, address: u64, data: &mut [u8]) -> Result<(), ArmError> {
        let address = valid_32bit_arm_address(address)?;

        // ST-Link V3 requires the data phase to be non-empty. For empty data,
        // return success.
        if data.is_empty() {
            return Ok(());
        }

        // Read needs to be chunked into chunks of appropriate max length of the probe
        let chunk_size = if self.probe.probe.hw_version < 3 {
            64
        } else {
            // This 128 byte chunk was set as the maximum possible amount is 255 even though it should
            // support 512 bytes in theory. Thus we chose a smaller amount to avoid more possible bugs
            // by not pushing the limit.
            // See code of `read_mem_8bit` for more info.
            128
        };

        for (index, chunk) in data.chunks_mut(chunk_size).enumerate() {
            chunk.copy_from_slice(&self.probe.probe.read_mem_8bit(
                address + (index * chunk_size) as u32,
                chunk.len() as u16,
                self.current_ap.ap_address().ap_v1()?,
            )?);
        }

        Ok(())
    }

    fn write_64(&mut self, address: u64, data: &[u64]) -> Result<(), ArmError> {
        let address = valid_32bit_arm_address(address)?;

        // ST-Link V3 requires the data phase to be non-empty. For empty data,
        // return success.
        if data.is_empty() {
            return Ok(());
        }

        let mut tx_buffer = vec![0u8; data.len() * 8];

        let mut offset = 0;

        for word in data {
            tx_buffer
                .gwrite(word, &mut offset)
                .expect("Failed to write into tx_buffer");
        }

        for (index, chunk) in tx_buffer.chunks(STLINK_MAX_WRITE_LEN).enumerate() {
            self.probe.probe.write_mem_32bit(
                address + (index * STLINK_MAX_WRITE_LEN) as u32,
                chunk,
                self.current_ap.ap_address().ap_v1()?,
            )?;
        }

        Ok(())
    }

    fn write_32(&mut self, address: u64, data: &[u32]) -> Result<(), ArmError> {
        let address = valid_32bit_arm_address(address)?;

        // ST-Link V3 requires the data phase to be non-empty. For empty data,
        // return success.
        if data.is_empty() {
            return Ok(());
        }

        let mut tx_buffer = vec![0u8; data.len() * 4];

        let mut offset = 0;

        for word in data {
            tx_buffer
                .gwrite(word, &mut offset)
                .expect("Failed to write into tx_buffer");
        }

        for (index, chunk) in tx_buffer.chunks(STLINK_MAX_WRITE_LEN).enumerate() {
            self.probe.probe.write_mem_32bit(
                address + (index * STLINK_MAX_WRITE_LEN) as u32,
                chunk,
                self.current_ap.ap_address().ap_v1()?,
            )?;
        }

        Ok(())
    }

    fn write_16(&mut self, address: u64, data: &[u16]) -> Result<(), ArmError> {
        let address = valid_32bit_arm_address(address)?;

        // ST-Link V3 requires the data phase to be non-empty. For empty data,
        // return success.
        if data.is_empty() {
            return Ok(());
        }

        let mut tx_buffer = vec![0u8; data.len() * 2];

        let mut offset = 0;

        for word in data {
            tx_buffer
                .gwrite(word, &mut offset)
                .expect("Failed to write into tx_buffer");
        }

        // use half the limits of 8bit accesses to be conservative. TODO can we increase this?
        let chunk_size = if self.probe.probe.hw_version < 3 {
            32
        } else {
            256
        };

        for (index, chunk) in tx_buffer.chunks(chunk_size).enumerate() {
            self.probe.probe.write_mem_16bit(
                address + (index * STLINK_MAX_WRITE_LEN) as u32,
                chunk,
                self.current_ap.ap_address().ap_v1()?,
            )?;
        }

        Ok(())
    }

    fn write_8(&mut self, address: u64, data: &[u8]) -> Result<(), ArmError> {
        let address = valid_32bit_arm_address(address)?;

        // ST-Link V3 requires the data phase to be non-empty. For empty data,
        // return success.
        if data.is_empty() {
            return Ok(());
        }

        // The underlying STLink command is limited to a single USB frame at a time
        // so we must manually chunk it into multiple command if it exceeds
        // that size.
        let chunk_size = if self.probe.probe.hw_version < 3 {
            64
        } else {
            512
        };

        // If we write less than 64 bytes, just write it directly
        if data.len() < chunk_size {
            tracing::trace!("write_8: small - direct 8 bit write to {:08x}", address);
            self.probe.probe.write_mem_8bit(
                address,
                data,
                self.current_ap.ap_address().ap_v1()?,
            )?;
        } else {
            // Handle unaligned data in the beginning.
            let bytes_beginning = if address % 4 == 0 {
                0
            } else {
                (4 - address % 4) as usize
            };

            let mut current_address = address;

            if bytes_beginning > 0 {
                tracing::trace!(
                    "write_8: at_begin - unaligned write of {} bytes to address {:08x}",
                    bytes_beginning,
                    current_address,
                );
                self.probe.probe.write_mem_8bit(
                    current_address,
                    &data[..bytes_beginning],
                    self.current_ap.ap_address().ap_v1()?,
                )?;

                current_address += bytes_beginning as u32;
            }

            // Address has to be aligned here.
            assert!(current_address % 4 == 0);

            let aligned_len = ((data.len() - bytes_beginning) / 4) * 4;

            tracing::trace!(
                "write_8: aligned write of {} bytes to address {:08x}",
                aligned_len,
                current_address,
            );

            for (index, chunk) in data[bytes_beginning..(bytes_beginning + aligned_len)]
                .chunks(STLINK_MAX_WRITE_LEN)
                .enumerate()
            {
                self.probe.probe.write_mem_32bit(
                    current_address + (index * STLINK_MAX_WRITE_LEN) as u32,
                    chunk,
                    self.current_ap.ap_address().ap_v1()?,
                )?;
            }

            current_address += aligned_len as u32;

            let remaining_bytes = &data[bytes_beginning + aligned_len..];

            if !remaining_bytes.is_empty() {
                tracing::trace!(
                    "write_8: at_end -unaligned write of {} bytes to address {:08x}",
                    bytes_beginning,
                    current_address,
                );
                self.probe.probe.write_mem_8bit(
                    current_address,
                    remaining_bytes,
                    self.current_ap.ap_address().ap_v1()?,
                )?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ArmError> {
        Ok(())
    }

    fn supports_8bit_transfers(&self) -> Result<bool, ArmError> {
        Ok(true)
    }
}

impl ArmMemoryInterface for StLinkMemoryInterface<'_> {
    fn base_address(&mut self) -> Result<u64, ArmError> {
        self.current_ap.base_address(self.probe)
    }

    fn fully_qualified_address(&self) -> FullyQualifiedApAddress {
        self.current_ap.ap_address().clone()
    }

    fn get_arm_debug_interface(&mut self) -> Result<&mut dyn ArmDebugInterface, DebugProbeError> {
        Ok(self.probe)
    }

    fn generic_status(&mut self) -> Result<crate::architecture::arm::ap::CSW, ArmError> {
        self.current_ap.generic_status(self.probe)
    }
}

fn is_wait_error(e: &StlinkError) -> bool {
    matches!(
        e,
        StlinkError::CommandFailed(Status::SwdDpWait | Status::SwdApWait)
    )
}

fn retry_on_wait<R>(mut f: impl FnMut() -> Result<R, StlinkError>) -> Result<R, StlinkError> {
    let mut last_err = None;
    for attempt in 0..13 {
        match f() {
            Ok(res) => return Ok(res),
            Err(e) => {
                if is_wait_error(&e) {
                    tracing::warn!("got SwdDpWait/SwdApWait, retrying.");
                    last_err = Some(e);
                } else {
                    return Err(e);
                }
            }
        }

        // Sleep with exponential backoff.
        thread::sleep(Duration::from_micros(100 << attempt));
    }

    tracing::warn!("too many retries, giving up");

    // Return the last error (will be SwdDpWait or SwdApWait)
    Err(last_err.unwrap())
}

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Debug)]
    struct MockUsb {
        hw_version: u8,
        jtag_version: u8,
        swim_version: u8,

        target_voltage_a0: f32,
        _target_voltage_a1: f32,
    }

    impl MockUsb {
        fn build(self) -> StLink<MockUsb> {
            StLink {
                device: self,
                name: "Mock STlink".into(),
                hw_version: 0,
                protocol: WireProtocol::Swd,
                jtag_version: 0,
                swd_speed_khz: 0,
                jtag_speed_khz: 0,
                swo_enabled: false,
                opened_aps: vec![],
            }
        }
    }

    impl StLinkUsb for MockUsb {
        fn write(
            &mut self,
            cmd: &[u8],
            _write_data: &[u8],
            read_data: &mut [u8],
            _timeout: Duration,
        ) -> Result<(), StlinkError> {
            match cmd[0] {
                commands::GET_VERSION => {
                    // GET_VERSION response structure:
                    //   Byte 0-1:
                    //     [15:12] Major/HW version
                    //     [11:6]  JTAG/SWD version
                    //     [5:0]   SWIM or MSC version
                    //   Byte 2-3: ST_VID
                    //   Byte 4-5: STLINK_PID

                    let version: u16 = ((self.hw_version as u16) << 12)
                        | ((self.jtag_version as u16) << 6)
                        | (self.swim_version as u16);

                    read_data[0] = (version >> 8) as u8;
                    read_data[1] = version as u8;

                    Ok(())
                }
                commands::GET_TARGET_VOLTAGE => {
                    read_data.pwrite(self.target_voltage_a0, 0).unwrap();
                    read_data.pwrite(self.target_voltage_a0, 4).unwrap();
                    Ok(())
                }
                commands::JTAG_COMMAND => {
                    // Return a status of OK for JTAG commands
                    read_data[0] = 0x80;

                    Ok(())
                }
                _ => Ok(()),
            }
        }
        fn reset(&mut self) -> Result<(), StlinkError> {
            Ok(())
        }

        fn read_swo(
            &mut self,
            _read_data: &mut [u8],
            _timeout: Duration,
        ) -> Result<usize, StlinkError> {
            unimplemented!("Not implemented for MockUSB")
        }
    }

    #[test]
    fn detect_old_firmware() {
        // Test that the init function detects old, unsupported firmware.

        let usb_mock = MockUsb {
            hw_version: 2,
            jtag_version: 20,
            swim_version: 0,

            target_voltage_a0: 1.0,
            _target_voltage_a1: 2.0,
        };

        let mut probe = usb_mock.build();

        let init_result = probe.init();

        match init_result.unwrap_err() {
            StlinkError::ProbeFirmwareOutdated(_) => (),
            other => panic!("Expected firmware outdated error, got {other}"),
        }
    }

    #[test]
    fn firmware_without_multiple_ap_support() {
        // Test that firmware with only support for a single AP works,
        // as long as only AP 0 is selected

        let usb_mock = MockUsb {
            hw_version: 2,
            jtag_version: 26,
            swim_version: 0,
            target_voltage_a0: 1.0,
            _target_voltage_a1: 2.0,
        };

        let mut probe = usb_mock.build();

        probe.init().expect("Init function failed");

        // Selecting AP 0 should still work
        probe.select_ap(0).expect("Select AP 0 failed.");

        probe
            .select_ap(1)
            .expect_err("Selecting AP other than AP 0 should fail");
    }

    #[test]
    fn firmware_with_multiple_ap_support() {
        // Test that firmware with only support for a single AP works,
        // as long as only AP 0 is selected

        let usb_mock = MockUsb {
            hw_version: 2,
            jtag_version: 30,
            swim_version: 0,
            target_voltage_a0: 1.0,
            _target_voltage_a1: 2.0,
        };

        let mut probe = usb_mock.build();

        probe.init().expect("Init function failed");

        // Selecting AP 0 should still work
        probe.select_ap(0).expect("Select AP 0 failed.");

        probe
            .select_ap(1)
            .expect("Selecting AP other than AP 0 should work");
    }

    #[test]
    fn test_is_wait_error() {
        assert!(!is_wait_error(&StlinkError::BanksNotAllowedOnDPRegister));
        assert!(!is_wait_error(&StlinkError::CommandFailed(
            Status::JtagFreqNotSupported
        )));
        assert!(is_wait_error(&StlinkError::CommandFailed(
            Status::SwdDpWait
        )));
        assert!(is_wait_error(&StlinkError::CommandFailed(
            Status::SwdApWait
        )));
    }
}
