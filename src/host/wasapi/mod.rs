pub use self::device::{Device, Devices};
use crate::traits::HostTrait;
use crate::BackendSpecificError;
use crate::DevicesError;
use std::io::Error as IoError;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use windows::Win32::Media::Audio;
use windows::Win32::System::Com;

mod com;
mod device;
mod stream;

/// The WASAPI host, the default windows host type.
///
/// Note: If you use a WASAPI output device as an input device it will
/// transparently enable loopback mode (see
/// https://docs.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording).
#[derive(Debug)]
pub struct Host;

impl Host {
    pub fn new() -> Result<Self, crate::HostUnavailable> {
        Ok(Host)
    }
}

impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;

    fn is_available() -> bool {
        // Assume WASAPI is always available on Windows.
        true
    }

    fn devices(&self) -> Result<Self::Devices, DevicesError> {
        Devices::new()
    }

    fn default_input_device(&self) -> Option<Self::Device> {
        Device::from_default_device(get_default_devices().input_device.clone())
    }

    fn default_output_device(&self) -> Option<Self::Device> {
        Device::from_default_device(get_default_devices().output_device.clone())
    }
}

impl From<windows::core::Error> for BackendSpecificError {
    fn from(error: windows::core::Error) -> Self {
        BackendSpecificError {
            description: format!("{}", IoError::from(error)),
        }
    }
}

trait ErrDeviceNotAvailable: From<BackendSpecificError> {
    fn device_not_available() -> Self;
}

impl ErrDeviceNotAvailable for crate::BuildStreamError {
    fn device_not_available() -> Self {
        Self::DeviceNotAvailable
    }
}

impl ErrDeviceNotAvailable for crate::SupportedStreamConfigsError {
    fn device_not_available() -> Self {
        Self::DeviceNotAvailable
    }
}

impl ErrDeviceNotAvailable for crate::DefaultStreamConfigError {
    fn device_not_available() -> Self {
        Self::DeviceNotAvailable
    }
}

impl ErrDeviceNotAvailable for crate::StreamError {
    fn device_not_available() -> Self {
        Self::DeviceNotAvailable
    }
}

fn windows_err_to_cpal_err<E: ErrDeviceNotAvailable>(e: windows::core::Error) -> E {
    windows_err_to_cpal_err_message::<E>(e, "")
}

fn windows_err_to_cpal_err_message<E: ErrDeviceNotAvailable>(
    e: windows::core::Error,
    message: &str,
) -> E {
    match e.code() {
        Audio::AUDCLNT_E_DEVICE_INVALIDATED => E::device_not_available(),
        _ => {
            let description = format!("{}{}", message, e);
            let err = BackendSpecificError { description };
            err.into()
        }
    }
}

// ------------

static ENUMERATOR: OnceLock<Enumerator> = OnceLock::new();

fn get_enumerator() -> &'static Enumerator {
    ENUMERATOR.get_or_init(|| {
        // COM initialization is thread local, but we only need to have COM initialized in the
        // thread we create the objects in
        com::com_initialized();

        // building the devices enumerator object
        unsafe {
            let enumerator = Com::CoCreateInstance::<_, Audio::IMMDeviceEnumerator>(
                &Audio::MMDeviceEnumerator,
                None,
                Com::CLSCTX_ALL,
            )
            .unwrap();

            Enumerator(enumerator)
        }
    })
}

/// Send/Sync wrapper around `IMMDeviceEnumerator`.
struct Enumerator(Audio::IMMDeviceEnumerator);

unsafe impl Send for Enumerator {}
unsafe impl Sync for Enumerator {}

// ------------

static DEFAULT_DEVICES: OnceLock<DefaultDevices> = OnceLock::new();

fn get_default_devices() -> &'static DefaultDevices {
    DEFAULT_DEVICES.get_or_init(|| DefaultDevices::new())
}

/// Keeps track of the existing default devices, as well as
/// managing the notification client.
struct DefaultDevices {
    input_device: Arc<Mutex<device::DefaultDevice>>,
    output_device: Arc<Mutex<device::DefaultDevice>>,
    notification_client: Audio::IMMNotificationClient,
}

unsafe impl Send for DefaultDevices {}
unsafe impl Sync for DefaultDevices {}

impl DefaultDevices {
    fn new() -> Self {
        fn build_default_device(
            flow: Audio::EDataFlow,
            role: Audio::ERole,
        ) -> Arc<Mutex<device::DefaultDevice>> {
            // Clippy rightfully points out that that `Mutex<DefaultDevice>` is not `Send` or `Sync`,
            // but it's unclear whether or not notifications are sent from a different thread,
            // so we're being conservative here and using `Arc`/`Mutex` anyway.
            #[allow(clippy::arc_with_non_send_sync)]
            Arc::new(Mutex::new(device::DefaultDevice::new(flow, role)))
        }
        let input_device = build_default_device(Audio::eCapture, Audio::eConsole);
        let output_device = build_default_device(Audio::eRender, Audio::eConsole);
        let notification_client = DeviceNotificationClient {
            input_device: input_device.clone(),
            output_device: output_device.clone(),
        };

        let notification_client = Audio::IMMNotificationClient::from(notification_client);
        unsafe {
            get_enumerator()
                .0
                .RegisterEndpointNotificationCallback(&notification_client)
                .ok();
        }
        dbg!("Host created");

        Self {
            input_device,
            output_device,
            notification_client,
        }
    }
}
impl Drop for DefaultDevices {
    fn drop(&mut self) {
        unsafe {
            dbg!("Unregistering notification client");
            get_enumerator()
                .0
                .UnregisterEndpointNotificationCallback(&self.notification_client)
                .ok();
        }
    }
}

#[derive(Clone)]
#[windows::core::implement(Audio::IMMNotificationClient)]
/// Used to update the default device when it changes. Only present when the device is a default device.
struct DeviceNotificationClient {
    input_device: Arc<Mutex<device::DefaultDevice>>,
    output_device: Arc<Mutex<device::DefaultDevice>>,
}

impl Audio::IMMNotificationClient_Impl for DeviceNotificationClient_Impl {
    fn OnDefaultDeviceChanged(
        &self,
        flow: Audio::EDataFlow,
        role: Audio::ERole,
        pwstrdefaultdeviceid: &windows_core::PCWSTR,
    ) -> windows_core::Result<()> {
        dbg!("OnDefaultDeviceChanged", flow, role, pwstrdefaultdeviceid);
        fn update_device(
            device: &mut device::DefaultDevice,
            flow: Audio::EDataFlow,
            role: Audio::ERole,
            pwstrdefaultdeviceid: &windows::core::PCWSTR,
        ) -> windows::core::Result<()> {
            if device.flow != flow && device.role != role {
                return Ok(());
            }

            if device.flow != flow && device.role != role {
                return Ok(());
            }

            if pwstrdefaultdeviceid.is_null() {
                device.device = None;
                return Ok(());
            }

            device.device = Some(unsafe { get_enumerator().0.GetDevice(*pwstrdefaultdeviceid)? });

            Ok(())
        }

        update_device(
            &mut self.input_device.lock().unwrap(),
            flow,
            role,
            pwstrdefaultdeviceid,
        )?;
        update_device(
            &mut self.output_device.lock().unwrap(),
            flow,
            role,
            pwstrdefaultdeviceid,
        )?;

        Ok(())
    }

    fn OnDeviceStateChanged(
        &self,
        _pwstrdeviceid: &windows_core::PCWSTR,
        _dwnewstate: Audio::DEVICE_STATE,
    ) -> windows_core::Result<()> {
        dbg!(_pwstrdeviceid, _dwnewstate);
        Ok(())
    }

    fn OnDeviceAdded(&self, _pwstrdeviceid: &windows_core::PCWSTR) -> windows_core::Result<()> {
        dbg!(_pwstrdeviceid);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _pwstrdeviceid: &windows_core::PCWSTR) -> windows_core::Result<()> {
        dbg!(_pwstrdeviceid);
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _pwstrdeviceid: &windows_core::PCWSTR,
        _key: &windows::Win32::Foundation::PROPERTYKEY,
    ) -> windows_core::Result<()> {
        dbg!(_pwstrdeviceid, _key);
        Ok(())
    }
}
