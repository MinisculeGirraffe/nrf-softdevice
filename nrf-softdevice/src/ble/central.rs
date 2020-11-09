//! Bluetooth Central operations. Central devices scan for advertisements from Peripheral devices and connect to them.
//!
//! Typically the Central device is the higher-powered device, such as a smartphone or laptop, since scanning is more
//! power-hungry than advertising.

use core::mem;
use core::ptr;

use crate::ble::{Address, Connection, ConnectionState};
use crate::raw;
use crate::util::*;
use crate::{RawError, Softdevice};

pub(crate) unsafe fn on_adv_report(_ble_evt: *const raw::ble_evt_t, _gap_evt: &raw::ble_gap_evt_t) {
    trace!("central on_adv_report");
}

pub(crate) unsafe fn on_qos_channel_survey_report(
    _ble_evt: *const raw::ble_evt_t,
    _gap_evt: &raw::ble_gap_evt_t,
) {
    trace!("central on_qos_channel_survey_report");
}

pub(crate) unsafe fn on_conn_param_update_request(
    _ble_evt: *const raw::ble_evt_t,
    _gap_evt: &raw::ble_gap_evt_t,
) {
    trace!("central on_conn_param_update_request");
}

#[derive(defmt::Format)]
pub enum ConnectError {
    Stopped,
    Raw(RawError),
}

impl From<RawError> for ConnectError {
    fn from(err: RawError) -> Self {
        ConnectError::Raw(err)
    }
}

#[derive(defmt::Format)]
pub enum ConnectStopError {
    NotRunning,
    Raw(RawError),
}

impl From<RawError> for ConnectStopError {
    fn from(err: RawError) -> Self {
        ConnectStopError::Raw(err)
    }
}

pub(crate) static CONNECT_SIGNAL: Signal<Result<Connection, ConnectError>> = Signal::new();
pub(crate) static MTU_UPDATED_SIGNAL: Signal<Result<(), ConnectError>> = Signal::new();

// Begins an ATT MTU exchange procedure, followed by a data length update request as necessary.
pub async fn connect(
    sd: &Softdevice,
    whitelist: &[Address],
    config: Config,
) -> Result<Connection, ConnectError> {
    let (addr, fp) = match whitelist.len() {
        0 => depanic!("zero-length whitelist"),
        1 => (
            &whitelist[0] as *const Address as *const raw::ble_gap_addr_t,
            raw::BLE_GAP_SCAN_FP_ACCEPT_ALL as u8,
        ),
        _ => depanic!("todo"),
    };

    // in units of 625us
    let scan_interval: u32 = 2732;
    let scan_window: u32 = 500;

    // TODO make configurable
    let mut scan_params: raw::ble_gap_scan_params_t = unsafe { mem::zeroed() };
    scan_params.set_extended(1);
    scan_params.set_active(1);
    scan_params.scan_phys = raw::BLE_GAP_PHY_1MBPS as u8;
    scan_params.set_filter_policy(fp);
    scan_params.timeout = 123;

    // s122 has these in us instead of 625us :shrug:
    #[cfg(not(feature = "s122"))]
    {
        scan_params.interval = scan_interval as u16;
        scan_params.window = scan_window as u16;
    }
    #[cfg(feature = "s122")]
    {
        scan_params.interval_us = scan_interval * 625;
        scan_params.window_us = scan_window * 625;
    }

    // TODO make configurable
    let mut conn_params: raw::ble_gap_conn_params_t = unsafe { mem::zeroed() };
    conn_params.min_conn_interval = 50;
    conn_params.max_conn_interval = 200;
    conn_params.slave_latency = 4;
    conn_params.conn_sup_timeout = 400; // 4 s

    let ret = unsafe { raw::sd_ble_gap_connect(addr, &mut scan_params, &mut conn_params, 1) };
    match RawError::convert(ret) {
        Ok(()) => {}
        Err(err) => {
            warn!("sd_ble_gap_connect err {:?}", err);
            return Err(ConnectError::Raw(err));
        }
    }

    info!("connect started");

    // TODO handle future drop

    let conn = CONNECT_SIGNAL.wait().await?;

    let state = conn.state();

    state.gap.update(|mut gap| {
        gap.rx_phys = config.tx_phys;
        gap.tx_phys = config.rx_phys;
        gap
    });

    let link = state.link.update(|mut link| {
        link.att_mtu_desired = config.att_mtu_desired;
        link
    });

    // Begin an ATT MTU exchange if necessary.
    if link.att_mtu_desired > link.att_mtu_effective as u16 {
        let ret = unsafe {
            raw::sd_ble_gattc_exchange_mtu_request(
                state.conn_handle.get().unwrap(), //todo
                link.att_mtu_desired,
            )
        };

        MTU_UPDATED_SIGNAL.wait().await?;

        if let Err(err) = RawError::convert(ret) {
            warn!("sd_ble_gattc_exchange_mtu_request err {:?}", err);
        }
    }

    // Send a data length update request if necessary.
    #[cfg(any(feature = "s113", feature = "s132", feature = "s140"))]
    {
        let link = state.link.update(|mut link| {
            link.data_length_desired = config.data_length_desired;
            link
        });

        if link.data_length_desired > link.data_length_effective {
            let dl_params = raw::ble_gap_data_length_params_t {
                max_rx_octets: link.data_length_desired.into(),
                max_tx_octets: link.data_length_desired.into(),
                max_rx_time_us: raw::BLE_GAP_DATA_LENGTH_AUTO as u16,
                max_tx_time_us: raw::BLE_GAP_DATA_LENGTH_AUTO as u16,
            };

            let ret = unsafe {
                raw::sd_ble_gap_data_length_update(
                    state.conn_handle.get().unwrap(), //todo
                    &dl_params as *const raw::ble_gap_data_length_params_t,
                    mem::zeroed(),
                )
            };

            if let Err(err) = RawError::convert(ret) {
                warn!("sd_ble_gap_data_length_update err {:?}", err);
            }
        }
    }

    Ok(conn)
}

pub fn connect_stop(sd: &Softdevice) -> Result<(), ConnectStopError> {
    let ret = unsafe { raw::sd_ble_gap_connect_cancel() };
    match RawError::convert(ret).dewarn(intern!("sd_ble_gap_connect_cancel")) {
        Ok(()) => Ok(()),
        Err(RawError::InvalidState) => Err(ConnectStopError::NotRunning),
        Err(e) => Err(e.into()),
    }
}

#[derive(Copy, Clone)]
pub struct Config {
    /// Requested ATT_MTU size for the next connection that is established.
    att_mtu_desired: u16,
    /// The stack's default data length. <27-251>
    #[cfg(any(feature = "s113", feature = "s132", feature = "s140"))]
    data_length_desired: u8,
    // bits of BLE_GAP_PHY_
    tx_phys: u8,
    // bits of BLE_GAP_PHY_
    rx_phys: u8,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            att_mtu_desired: 32,
            #[cfg(any(feature = "s113", feature = "s132", feature = "s140"))]
            data_length_desired: 27,
            tx_phys: raw::BLE_GAP_PHY_AUTO as u8,
            rx_phys: raw::BLE_GAP_PHY_AUTO as u8,
        }
    }
}
