//! Bluetooth Central operations. Central devices scan for advertisements from Peripheral devices and connect to them.
//!
//! Typically the Central device is the higher-powered device, such as a smartphone or laptop, since scanning is more
//! power-hungry than advertising.

use core::mem;
use core::ptr;
use core::slice;

#[cfg(feature = "ble-gatt-client")]
use crate::ble::gatt_client;
use crate::ble::{Address, Connection, ConnectionState};
use crate::raw;
use crate::util::{panic, *};
use crate::{RawError, Softdevice};

pub(crate) unsafe fn on_adv_report(ble_evt: *const raw::ble_evt_t, _gap_evt: &raw::ble_gap_evt_t) {
    trace!("central on_adv_report");
    SCAN_PORTAL.call(ScanPortalMessage::AdvReport(ble_evt))
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
    Timeout,
    Raw(RawError),
}

impl From<RawError> for ConnectError {
    fn from(err: RawError) -> Self {
        ConnectError::Raw(err)
    }
}

pub(crate) static CONNECT_PORTAL: Portal<Result<Connection, ConnectError>> = Portal::new();

// Begins an ATT MTU exchange procedure, followed by a data length update request as necessary.
pub async fn connect(
    sd: &Softdevice,
    whitelist: &[Address],
    config: &Config,
) -> Result<Connection, ConnectError> {
    let (addr, fp) = match whitelist.len() {
        0 => panic!("zero-length whitelist"),
        1 => (
            &whitelist[0] as *const Address as *const raw::ble_gap_addr_t,
            raw::BLE_GAP_SCAN_FP_ACCEPT_ALL as u8,
        ),
        _ => panic!("todo"),
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
    scan_params.timeout = raw::BLE_GAP_SCAN_TIMEOUT_UNLIMITED as _;

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

    let d = OnDrop::new(|| {
        let ret = unsafe { raw::sd_ble_gap_connect_cancel() };
        if let Err(e) = RawError::convert(ret) {
            warn!("sd_ble_gap_connect_cancel: {:?}", e);
        }
    });

    let ret = unsafe { raw::sd_ble_gap_connect(addr, &mut scan_params, &config.conn_params, 1) };
    if let Err(err) = RawError::convert(ret) {
        warn!("sd_ble_gap_connect err {:?}", err);
        return Err(err.into());
    }

    info!("connect started");

    let conn = CONNECT_PORTAL.wait_once(|res| res).await?;

    conn.with_state(|state| {
        state.rx_phys = config.tx_phys;
        state.tx_phys = config.rx_phys;
    });

    d.defuse();

    #[cfg(feature = "ble-gatt-client")]
    {
        let mtu = config.att_mtu.unwrap_or(sd.att_mtu);
        unwrap!(crate::ble::gatt_client::att_mtu_exchange(&conn, mtu).await);
    }

    Ok(conn)
}

#[derive(Copy, Clone)]
pub struct Config {
    /// Requested ATT_MTU size for the next connection that is established.
    #[cfg(feature = "ble-gatt-client")]
    pub att_mtu: Option<u16>,
    // bits of BLE_GAP_PHY_
    pub tx_phys: u8,
    // bits of BLE_GAP_PHY_
    pub rx_phys: u8,

    pub conn_params: raw::ble_gap_conn_params_t,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            #[cfg(feature = "ble-gatt-client")]
            att_mtu: None,
            tx_phys: raw::BLE_GAP_PHY_AUTO as _,
            rx_phys: raw::BLE_GAP_PHY_AUTO as _,
            conn_params: raw::ble_gap_conn_params_t {
                min_conn_interval: 40,
                max_conn_interval: 200,
                slave_latency: 0,
                conn_sup_timeout: 400, // 4s
            },
        }
    }
}

#[derive(defmt::Format)]
pub enum ScanError {
    Timeout,
    Raw(RawError),
}

impl From<RawError> for ScanError {
    fn from(err: RawError) -> Self {
        ScanError::Raw(err)
    }
}

pub(crate) enum ScanPortalMessage {
    Timeout(*const raw::ble_evt_t),
    AdvReport(*const raw::ble_evt_t),
}

pub(crate) static SCAN_PORTAL: Portal<ScanPortalMessage> = Portal::new();

pub async fn scan<'a, F, R>(
    sd: &Softdevice,
    config: &ScanConfig<'a>,
    mut f: F,
) -> Result<R, ScanError>
where
    F: for<'b> FnMut(&'b raw::ble_gap_evt_adv_report_t) -> Option<R>,
{
    // in units of 625us
    let scan_interval: u32 = 2732;
    let scan_window: u32 = 500;

    // TODO make configurable
    let mut scan_params: raw::ble_gap_scan_params_t = unsafe { mem::zeroed() };
    scan_params.set_extended(1);
    scan_params.set_active(1);
    scan_params.scan_phys = raw::BLE_GAP_PHY_1MBPS as u8;
    scan_params.set_filter_policy(raw::BLE_GAP_SCAN_FP_ACCEPT_ALL as _); // todo
    scan_params.timeout = raw::BLE_GAP_SCAN_TIMEOUT_UNLIMITED as _;

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

    // Buffer to store received advertisement data.
    const BUF_LEN: usize = 256;
    let mut buf = [0u8; BUF_LEN];
    let buf_data = raw::ble_data_t {
        p_data: buf.as_mut_ptr(),
        len: BUF_LEN as u16,
    };

    let ret = unsafe { raw::sd_ble_gap_scan_start(&scan_params, &buf_data) };
    match RawError::convert(ret) {
        Ok(()) => {}
        Err(err) => {
            warn!("sd_ble_gap_scan_start err {:?}", err);
            return Err(ScanError::Raw(err));
        }
    }

    let d = OnDrop::new(|| {
        let ret = unsafe { raw::sd_ble_gap_scan_stop() };
        if let Err(e) = RawError::convert(ret) {
            warn!("sd_ble_gap_scan_stop: {:?}", e);
        }
    });

    info!("Scan started");
    let res = SCAN_PORTAL
        .wait_many(|msg| match msg {
            ScanPortalMessage::Timeout(ble_evt) => return Some(Err(ScanError::Timeout)),
            ScanPortalMessage::AdvReport(ble_evt) => unsafe {
                let gap_evt = get_union_field(ble_evt, &(*ble_evt).evt.gap_evt);
                let params = &gap_evt.params.adv_report;
                if let Some(r) = f(params) {
                    return Some(Ok(r));
                }

                // Resume scan
                let ret = raw::sd_ble_gap_scan_start(ptr::null(), &buf_data);
                match RawError::convert(ret) {
                    Ok(()) => {}
                    Err(err) => {
                        warn!("sd_ble_gap_scan_start err {:?}", err);
                        return Some(Err(ScanError::Raw(err)));
                    }
                };
                None
            },
        })
        .await?;

    Ok(res)
}

#[derive(Copy, Clone)]
pub struct ScanConfig<'a> {
    pub whitelist: Option<&'a [Address]>,
}

impl<'a> Default for ScanConfig<'a> {
    fn default() -> Self {
        Self { whitelist: None }
    }
}
