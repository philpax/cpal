use crate::host::wasapi::stream::{config_to_waveformatextensible, StreamInnerDeviceState};
use crate::FrameCount;
use crate::{
    BackendSpecificError, BufferSize, Data, DefaultStreamConfigError, DeviceNameError,
    DevicesError, InputCallbackInfo, OutputCallbackInfo, SampleFormat, SampleRate, StreamConfig,
    SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
    SupportedStreamConfigsError, COMMON_SAMPLE_RATES,
};
use std::ffi::OsString;
use std::fmt;
use std::os::windows::ffi::OsStringExt;
use std::ptr;
use std::slice;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::com;
use super::windows_err_to_cpal_err;
use windows::core::Interface;
use windows::core::GUID;
use windows::Win32::Devices::Properties;
use windows::Win32::Foundation;
use windows::Win32::Media::{Audio, KernelStreaming, Multimedia};
use windows::Win32::System::Com;
use windows::Win32::System::Com::{StructuredStorage, STGM_READ};
use windows::Win32::System::Variant::VT_LPWSTR;

use super::get_enumerator;
use super::stream::{Stream, StreamInner};
use crate::{traits::DeviceTrait, BuildStreamError, StreamError};

pub type SupportedInputConfigs = std::vec::IntoIter<SupportedStreamConfigRange>;
pub type SupportedOutputConfigs = std::vec::IntoIter<SupportedStreamConfigRange>;

/// Wrapper because of that stupid decision to remove `Send` and `Sync` from raw pointers.
#[derive(Clone)]
struct IAudioClientWrapper(Audio::IAudioClient);
unsafe impl Send for IAudioClientWrapper {}
unsafe impl Sync for IAudioClientWrapper {}

/// An opaque type that identifies an end point.
#[derive(Clone)]
pub struct Device {
    device: DeviceType,
    /// We cache an uninitialized `IAudioClient` so that we can call functions from it without
    /// having to create/destroy audio clients all the time.
    future_audio_client: Arc<Mutex<Option<IAudioClientWrapper>>>, // TODO: add NonZero around the ptr
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn name(&self) -> Result<String, DeviceNameError> {
        Device::name(self)
    }

    fn supports_input(&self) -> bool {
        self.data_flow() == Some(Audio::eCapture)
    }

    fn supports_output(&self) -> bool {
        self.data_flow() == Some(Audio::eRender)
    }

    fn supported_input_configs(
        &self,
    ) -> Result<Self::SupportedInputConfigs, SupportedStreamConfigsError> {
        Device::supported_input_configs(self)
    }

    fn supported_output_configs(
        &self,
    ) -> Result<Self::SupportedOutputConfigs, SupportedStreamConfigsError> {
        Device::supported_output_configs(self)
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Device::default_input_config(self)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        Device::default_output_config(self)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        let stream_inner = self.build_input_stream_raw_inner(config, sample_format)?;
        Ok(Stream::new_input(
            stream_inner,
            data_callback,
            error_callback,
        ))
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        _timeout: Option<Duration>,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        let stream_inner = self.build_output_stream_raw_inner(config, sample_format)?;
        Ok(Stream::new_output(
            stream_inner,
            data_callback,
            error_callback,
        ))
    }
}

#[derive(Clone, Debug)]
enum DeviceType {
    Device(Audio::IMMDevice),
    Default(Arc<Mutex<DefaultDevice>>),
}
impl DeviceType {
    pub fn get(&self) -> Option<Audio::IMMDevice> {
        match self {
            DeviceType::Device(device) => Some(device.clone()),
            DeviceType::Default(device) => device.lock().ok().and_then(|d| d.device.clone()),
        }
    }
}

/// A wrapper around the default device that keeps track of the desired flow/role,
/// so that it can be updated when the default device changes.
#[derive(Debug)]
pub(super) struct DefaultDevice {
    pub flow: Audio::EDataFlow,
    pub role: Audio::ERole,
    pub device: Option<Audio::IMMDevice>,
}

impl DefaultDevice {
    pub fn new(flow: Audio::EDataFlow, role: Audio::ERole) -> Self {
        let device = unsafe { get_enumerator().0.GetDefaultAudioEndpoint(flow, role).ok() };
        Self { flow, role, device }
    }
}

struct Endpoint {
    endpoint: Audio::IMMEndpoint,
}

// Use RAII to make sure CoTaskMemFree is called when we are responsible for freeing.
struct WaveFormatExPtr(*mut Audio::WAVEFORMATEX);

impl Drop for WaveFormatExPtr {
    fn drop(&mut self) {
        unsafe {
            Com::CoTaskMemFree(Some(self.0 as *mut _));
        }
    }
}

unsafe fn immendpoint_from_immdevice(device: Audio::IMMDevice) -> Audio::IMMEndpoint {
    device
        .cast::<Audio::IMMEndpoint>()
        .expect("could not query IMMDevice interface for IMMEndpoint")
}

unsafe fn data_flow_from_immendpoint(endpoint: &Audio::IMMEndpoint) -> Audio::EDataFlow {
    endpoint
        .GetDataFlow()
        .expect("could not get endpoint data_flow")
}

// Given the audio client and format, returns whether or not the format is supported.
pub unsafe fn is_format_supported(
    client: &Audio::IAudioClient,
    waveformatex_ptr: *const Audio::WAVEFORMATEX,
) -> Result<bool, SupportedStreamConfigsError> {
    // Check if the given format is supported.
    let mut closest_waveformatex_ptr: *mut Audio::WAVEFORMATEX = ptr::null_mut();

    let result = client.IsFormatSupported(
        Audio::AUDCLNT_SHAREMODE_SHARED,
        waveformatex_ptr,
        Some(&mut closest_waveformatex_ptr as *mut _),
    );

    if !closest_waveformatex_ptr.is_null() {
        Com::CoTaskMemFree(Some(closest_waveformatex_ptr as *mut std::ffi::c_void));
    }

    // `IsFormatSupported` can return `S_FALSE` (which means that a compatible format
    // has been found, but not an exact match) so we also treat this as unsupported.
    match result {
        Audio::AUDCLNT_E_DEVICE_INVALIDATED => Err(SupportedStreamConfigsError::DeviceNotAvailable),
        r if r.is_err() => Ok(false),
        Foundation::S_FALSE => Ok(false),
        _ => Ok(true),
    }
}

// Get a cpal Format from a WAVEFORMATEX.
unsafe fn format_from_waveformatex_ptr(
    waveformatex_ptr: *const Audio::WAVEFORMATEX,
    audio_client: &Audio::IAudioClient,
) -> Option<SupportedStreamConfig> {
    fn cmp_guid(a: &GUID, b: &GUID) -> bool {
        (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
    }
    let sample_format = match (
        (*waveformatex_ptr).wBitsPerSample,
        (*waveformatex_ptr).wFormatTag as u32,
    ) {
        (8, Audio::WAVE_FORMAT_PCM) => SampleFormat::U8,
        (16, Audio::WAVE_FORMAT_PCM) => SampleFormat::I16,
        (32, Multimedia::WAVE_FORMAT_IEEE_FLOAT) => SampleFormat::F32,
        (n_bits, KernelStreaming::WAVE_FORMAT_EXTENSIBLE) => {
            let waveformatextensible_ptr = waveformatex_ptr as *const Audio::WAVEFORMATEXTENSIBLE;
            let sub = (*waveformatextensible_ptr).SubFormat;

            if cmp_guid(&sub, &KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM) {
                match n_bits {
                    8 => SampleFormat::U8,
                    16 => SampleFormat::I16,
                    24 => SampleFormat::I24,
                    32 => SampleFormat::I32,
                    64 => SampleFormat::I64,
                    _ => return None,
                }
            } else if n_bits == 32 && cmp_guid(&sub, &Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT) {
                SampleFormat::F32
            } else {
                return None;
            }
        }
        // Unknown data format returned by GetMixFormat.
        _ => return None,
    };

    let sample_rate = SampleRate((*waveformatex_ptr).nSamplesPerSec);

    // GetBufferSizeLimits is only used for Hardware-Offloaded Audio
    // Processing, which was added in Windows 8, which places hardware
    // limits on the size of the audio buffer. If the sound system
    // *isn't* using offloaded audio, we're using a software audio
    // processing stack and have pretty much free rein to set buffer
    // size.
    //
    // In software audio stacks GetBufferSizeLimits returns
    // AUDCLNT_E_OFFLOAD_MODE_ONLY.
    //
    // https://docs.microsoft.com/en-us/windows-hardware/drivers/audio/hardware-offloaded-audio-processing
    let (mut min_buffer_duration, mut max_buffer_duration) = (0, 0);
    let buffer_size_is_limited = audio_client
        .cast::<Audio::IAudioClient2>()
        .and_then(|audio_client| {
            audio_client.GetBufferSizeLimits(
                waveformatex_ptr,
                true,
                &mut min_buffer_duration,
                &mut max_buffer_duration,
            )
        })
        .is_ok();
    let buffer_size = if buffer_size_is_limited {
        SupportedBufferSize::Range {
            min: buffer_duration_to_frames(min_buffer_duration, sample_rate.0),
            max: buffer_duration_to_frames(max_buffer_duration, sample_rate.0),
        }
    } else {
        SupportedBufferSize::Range {
            min: 0,
            max: u32::MAX,
        }
    };

    let format = SupportedStreamConfig {
        channels: (*waveformatex_ptr).nChannels as _,
        sample_rate,
        buffer_size,
        sample_format,
    };
    Some(format)
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    pub fn name(&self) -> Result<String, DeviceNameError> {
        unsafe {
            // Open the device's property store.
            let property_store = self
                .device
                .get()
                .ok_or_else(|| {
                    DeviceNameError::from(BackendSpecificError {
                        description: "device not found".to_string(),
                    })
                })?
                .OpenPropertyStore(STGM_READ)
                .expect("could not open property store");

            // Get the endpoint's friendly-name property.
            let mut property_value = property_store
                .GetValue(&Properties::DEVPKEY_Device_FriendlyName as *const _ as *const _)
                .map_err(|err| {
                    let description =
                        format!("failed to retrieve name from property store: {}", err);
                    let err = BackendSpecificError { description };
                    DeviceNameError::from(err)
                })?;

            let prop_variant = &property_value.Anonymous.Anonymous;

            // Read the friendly-name from the union data field, expecting a *const u16.
            if prop_variant.vt != VT_LPWSTR {
                let description = format!(
                    "property store produced invalid data: {:?}",
                    prop_variant.vt
                );
                let err = BackendSpecificError { description };
                return Err(err.into());
            }
            let ptr_utf16 = *(&prop_variant.Anonymous as *const _ as *const *const u16);

            // Find the length of the friendly name.
            let mut len = 0;
            while *ptr_utf16.offset(len) != 0 {
                len += 1;
            }

            // Create the utf16 slice and convert it into a string.
            let name_slice = slice::from_raw_parts(ptr_utf16, len as usize);
            let name_os_string: OsString = OsStringExt::from_wide(name_slice);
            let name_string = match name_os_string.into_string() {
                Ok(string) => string,
                Err(os_string) => os_string.to_string_lossy().into(),
            };

            // Clean up the property.
            StructuredStorage::PropVariantClear(&mut property_value).ok();

            Ok(name_string)
        }
    }

    #[inline]
    fn from_immdevice(device: Audio::IMMDevice) -> Self {
        Device {
            device: DeviceType::Device(device),
            future_audio_client: Arc::new(Mutex::new(None)),
        }
    }

    #[inline]
    pub(super) fn from_default_device(device: Arc<Mutex<DefaultDevice>>) -> Option<Self> {
        // If the device is not currently available, return None.
        device.lock().unwrap().device.as_ref()?;
        Some(Device {
            device: DeviceType::Default(device),
            future_audio_client: Arc::new(Mutex::new(None)),
        })
    }

    /// Ensures that `future_audio_client` contains a `Some` and returns a locked mutex to it.
    fn ensure_future_audio_client(
        &self,
    ) -> Result<MutexGuard<'_, Option<IAudioClientWrapper>>, windows::core::Error> {
        let mut lock = self.future_audio_client.lock().unwrap();
        if lock.is_some() {
            return Ok(lock);
        }

        let audio_client: Audio::IAudioClient = unsafe {
            // can fail if the device has been disconnected since we enumerated it, or if
            // the device doesn't support playback for some reason
            let device = self.device.get().ok_or_else(|| {
                windows::core::Error::new(
                    Audio::AUDCLNT_E_DEVICE_INVALIDATED,
                    "device not found when ensuring future audio client",
                )
            })?;
            device.Activate(Com::CLSCTX_ALL, None)?
        };

        *lock = Some(IAudioClientWrapper(audio_client));
        Ok(lock)
    }

    /// Returns an uninitialized `IAudioClient`.
    #[inline]
    pub(crate) fn build_audioclient(&self) -> Result<Audio::IAudioClient, windows::core::Error> {
        let mut lock = self.ensure_future_audio_client()?;
        Ok(lock.take().unwrap().0)
    }

    // There is no way to query the list of all formats that are supported by the
    // audio processor, so instead we just trial some commonly supported formats.
    //
    // Common formats are trialed by first getting the default format (returned via
    // `GetMixFormat`) and then mutating that format with common sample rates and
    // querying them via `IsFormatSupported`.
    //
    // When calling `IsFormatSupported` with the shared-mode audio engine, only the default
    // number of channels seems to be supported. Any, more or less returns an invalid
    // parameter error. Thus, we just assume that the default number of channels is the only
    // number supported.
    fn supported_formats(&self) -> Result<SupportedInputConfigs, SupportedStreamConfigsError> {
        // initializing COM because we call `CoTaskMemFree` to release the format.
        com::com_initialized();

        // Retrieve the `IAudioClient`.
        let lock = match self.ensure_future_audio_client() {
            Ok(lock) => lock,
            Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(SupportedStreamConfigsError::DeviceNotAvailable)
            }
            Err(e) => {
                let description = format!("{}", e);
                let err = BackendSpecificError { description };
                return Err(err.into());
            }
        };
        let client = &lock.as_ref().unwrap().0;

        unsafe {
            // Retrieve the pointer to the default WAVEFORMATEX.
            let default_waveformatex_ptr = client
                .GetMixFormat()
                .map(WaveFormatExPtr)
                .map_err(windows_err_to_cpal_err::<SupportedStreamConfigsError>)?;

            // If the default format can't succeed we have no hope of finding other formats.
            if !is_format_supported(client, default_waveformatex_ptr.0)? {
                let description =
                    "Could not determine support for default `WAVEFORMATEX`".to_string();
                let err = BackendSpecificError { description };
                return Err(err.into());
            }

            let format = match format_from_waveformatex_ptr(default_waveformatex_ptr.0, client) {
                Some(fmt) => fmt,
                None => {
                    let description =
                        "could not create a `cpal::SupportedStreamConfig` from a `WAVEFORMATEX`"
                            .to_string();
                    let err = BackendSpecificError { description };
                    return Err(err.into());
                }
            };

            let mut sample_rates: Vec<SampleRate> = COMMON_SAMPLE_RATES.to_vec();

            if !sample_rates.contains(&format.sample_rate) {
                sample_rates.push(format.sample_rate)
            }

            let mut supported_formats = Vec::new();

            for sample_rate in sample_rates {
                for sample_format in [
                    SampleFormat::U8,
                    SampleFormat::I16,
                    SampleFormat::I24,
                    SampleFormat::U24,
                    SampleFormat::I32,
                    SampleFormat::I64,
                    SampleFormat::F32,
                ] {
                    if let Some(waveformat) = config_to_waveformatextensible(
                        &StreamConfig {
                            channels: format.channels,
                            sample_rate,
                            buffer_size: BufferSize::Default,
                        },
                        sample_format,
                    ) {
                        if is_format_supported(
                            client,
                            &waveformat.Format as *const Audio::WAVEFORMATEX,
                        )? {
                            supported_formats.push(SupportedStreamConfigRange {
                                channels: format.channels,
                                min_sample_rate: sample_rate,
                                max_sample_rate: sample_rate,
                                buffer_size: format.buffer_size,
                                sample_format,
                            })
                        }
                    }
                }
            }
            Ok(supported_formats.into_iter())
        }
    }

    pub fn supported_input_configs(
        &self,
    ) -> Result<SupportedInputConfigs, SupportedStreamConfigsError> {
        if self.data_flow() == Some(Audio::eCapture) {
            self.supported_formats()
        // If it's an output device, assume no input formats.
        } else {
            Ok(vec![].into_iter())
        }
    }

    pub fn supported_output_configs(
        &self,
    ) -> Result<SupportedOutputConfigs, SupportedStreamConfigsError> {
        if self.data_flow() == Some(Audio::eRender) {
            self.supported_formats()
        // If it's an input device, assume no output formats.
        } else {
            Ok(vec![].into_iter())
        }
    }

    // We always create voices in shared mode, therefore all samples go through an audio
    // processor to mix them together.
    //
    // One format is guaranteed to be supported, the one returned by `GetMixFormat`.
    fn default_format(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        // initializing COM because we call `CoTaskMemFree`
        com::com_initialized();

        let lock = match self.ensure_future_audio_client() {
            Ok(lock) => lock,
            Err(ref e) if e.code() == Audio::AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(DefaultStreamConfigError::DeviceNotAvailable)
            }
            Err(e) => {
                let description = format!("{}", e);
                let err = BackendSpecificError { description };
                return Err(err.into());
            }
        };
        let client = &lock.as_ref().unwrap().0;

        unsafe {
            let format_ptr = client
                .GetMixFormat()
                .map(WaveFormatExPtr)
                .map_err(windows_err_to_cpal_err::<DefaultStreamConfigError>)?;

            format_from_waveformatex_ptr(format_ptr.0, client)
                .ok_or(DefaultStreamConfigError::StreamTypeNotSupported)
        }
    }

    pub(crate) fn data_flow(&self) -> Option<Audio::EDataFlow> {
        let endpoint = Endpoint::from(self.device.get()?);
        Some(endpoint.data_flow())
    }

    pub fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        if self.data_flow() == Some(Audio::eCapture) {
            self.default_format()
        } else {
            Err(DefaultStreamConfigError::StreamTypeNotSupported)
        }
    }

    pub fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        let data_flow = self.data_flow();
        if data_flow == Some(Audio::eRender) {
            self.default_format()
        } else {
            Err(DefaultStreamConfigError::StreamTypeNotSupported)
        }
    }

    pub(crate) fn build_input_stream_raw_inner(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
    ) -> Result<StreamInner, BuildStreamError> {
        // Making sure that COM is initialized.
        // It's not actually sure that this is required, but when in doubt do it.
        com::com_initialized();

        let audio_client = self
            .build_audioclient()
            .map_err(windows_err_to_cpal_err::<BuildStreamError>)?;

        let device_state = StreamInnerDeviceState::build_for_input(
            audio_client,
            self.data_flow(),
            config,
            sample_format,
        )?;

        Ok(StreamInner {
            device_state,
            playing: false,
            config: config.clone(),
            sample_format,
        })
    }

    pub(crate) fn build_output_stream_raw_inner(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
    ) -> Result<StreamInner, BuildStreamError> {
        // Making sure that COM is initialized.
        // It's not actually sure that this is required, but when in doubt do it.
        com::com_initialized();

        let audio_client = self
            .build_audioclient()
            .map_err(windows_err_to_cpal_err::<BuildStreamError>)?;

        let device_state =
            StreamInnerDeviceState::build_for_output(audio_client, config, sample_format)?;

        Ok(StreamInner {
            device_state,
            playing: false,
            config: config.clone(),
            sample_format,
        })
    }
}

impl PartialEq for Device {
    #[inline]
    fn eq(&self, other: &Device) -> bool {
        let device1 = self.device.get();
        let device2 = other.device.get();

        match (device1, device2) {
            (Some(device1), Some(device2)) => {
                // Use case: In order to check whether the default device has changed
                // the client code might need to compare the previous default device with the current one.
                // The pointer comparison (`self.device == other.device`) don't work there,
                // because the pointers are different even when the default device stays the same.
                //
                // In this code section we're trying to use the GetId method for the device comparison, cf.
                // https://docs.microsoft.com/en-us/windows/desktop/api/mmdeviceapi/nf-mmdeviceapi-immdevice-getid
                unsafe {
                    struct IdRAII(windows::core::PWSTR);
                    /// RAII for device IDs.
                    impl Drop for IdRAII {
                        fn drop(&mut self) {
                            unsafe { Com::CoTaskMemFree(Some(self.0 .0 as *mut _)) }
                        }
                    }
                    // GetId only fails with E_OUTOFMEMORY and if it does, we're probably dead already.
                    // Plus it won't do to change the device comparison logic unexpectedly.
                    let id1 = device1.GetId().expect("cpal: GetId failure");
                    let id1 = IdRAII(id1);
                    let id2 = device2.GetId().expect("cpal: GetId failure");
                    let id2 = IdRAII(id2);
                    // 16-bit null-terminated comparison.
                    let mut offset = 0;
                    loop {
                        let w1: u16 = *(id1.0).0.offset(offset);
                        let w2: u16 = *(id2.0).0.offset(offset);
                        if w1 == 0 && w2 == 0 {
                            return true;
                        }
                        if w1 != w2 {
                            return false;
                        }
                        offset += 1;
                    }
                }
            }
            (None, None) => true,
            (Some(_), None) | (None, Some(_)) => false,
        }
    }
}

impl Eq for Device {}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Device")
            .field("device", &self.device)
            .field("name", &self.name())
            .finish()
    }
}

impl From<Audio::IMMDevice> for Endpoint {
    fn from(device: Audio::IMMDevice) -> Self {
        unsafe {
            let endpoint = immendpoint_from_immdevice(device);
            Endpoint { endpoint }
        }
    }
}

impl Endpoint {
    fn data_flow(&self) -> Audio::EDataFlow {
        unsafe { data_flow_from_immendpoint(&self.endpoint) }
    }
}

/// WASAPI implementation for `Devices`.
pub struct Devices {
    collection: Audio::IMMDeviceCollection,
    total_count: u32,
    next_item: u32,
}

impl Devices {
    pub fn new() -> Result<Self, DevicesError> {
        unsafe {
            // can fail because of wrong parameters (should never happen) or out of memory
            let collection = get_enumerator()
                .0
                .EnumAudioEndpoints(Audio::eAll, Audio::DEVICE_STATE_ACTIVE)
                .map_err(BackendSpecificError::from)?;

            let count = collection.GetCount().map_err(BackendSpecificError::from)?;

            Ok(Devices {
                collection,
                total_count: count,
                next_item: 0,
            })
        }
    }
}

unsafe impl Send for Devices {}
unsafe impl Sync for Devices {}

impl Iterator for Devices {
    type Item = Device;

    fn next(&mut self) -> Option<Device> {
        if self.next_item >= self.total_count {
            return None;
        }

        unsafe {
            let device = self.collection.Item(self.next_item).unwrap();
            self.next_item += 1;
            Some(Device::from_immdevice(device))
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let num = self.total_count - self.next_item;
        let num = num as usize;
        (num, Some(num))
    }
}

fn buffer_duration_to_frames(buffer_duration: i64, sample_rate: u32) -> FrameCount {
    (buffer_duration * sample_rate as i64 * 100 / 1_000_000_000) as FrameCount
}
