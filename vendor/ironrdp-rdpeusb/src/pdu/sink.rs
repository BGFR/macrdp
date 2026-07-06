//! Messages specific to the [Device Sink][1] interface.
//!
//! Identified by the default interface ID `0x00000001`, this interface is used by the client to
//! communicate with the server about new USB devices.
//!
//! [1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeusb/a9a8add7-4e99-4697-abd0-ad64c80c788d

use alloc::format;

use ironrdp_core::{
    Decode, DecodeOwned as _, DecodeResult, Encode, EncodeResult, ReadCursor, WriteCursor, ensure_fixed_part_size,
    ensure_size, other_err, unsupported_value_err,
};
use ironrdp_pdu::utils::strict_sum;
use ironrdp_str::multi_sz::MultiSzString;
use ironrdp_str::prefixed::Cch32String;

use crate::pdu::header::{FunctionId, InterfaceId, Mask, MessageId, SharedMsgHeader};

/// [\[MS-RDPEUSB\] 2.2.4.1 Add Virtual Channel Message (ADD_VIRTUAL_CHANNEL)][1] packet.
///
/// Sent from the client to the server to create a new instance of dynamic virtual channel.
///
/// [1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeusb/5b6005ed-03a6-4c70-9513-07a571367337
#[doc(alias = "ADD_VIRTUAL_CHANNEL")]
#[derive(Debug, PartialEq)]
pub struct AddVirtualChannel {
    pub msg_id: MessageId,
}

impl AddVirtualChannel {
    pub const FIXED_PART_SIZE: usize = SharedMsgHeader::SIZE_REQ /* Header */;

    pub fn header(&self) -> SharedMsgHeader {
        SharedMsgHeader {
            interface_id: InterfaceId::DEVICE_SINK,
            mask: Mask::StreamIdProxy,
            msg_id: self.msg_id,
            function_id: Some(FunctionId::ADD_VIRTUAL_CHANNEL),
        }
    }

    pub(crate) fn decode(_: &mut ReadCursor<'_>, header: SharedMsgHeader) -> DecodeResult<Self> {
        Ok(Self { msg_id: header.msg_id })
    }
}

impl Encode for AddVirtualChannel {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        self.header().encode(dst)
    }

    fn name(&self) -> &'static str {
        "ADD_VIRTUAL_CHANNEL"
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

/// [\[MS-RDPEUSB\] 2.2.4.2 Add Device Message (ADD_DEVICE)][1] packet.
///
/// Sent from the client to the server in order to create a redirected USB device on the server.
///
/// [1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeusb/a26bcb6d-d45d-48a9-b9bd-22e0107d8393
#[doc(alias = "ADD_DEVICE")]
#[derive(Debug, PartialEq)]
pub struct AddDevice {
    pub msg_id: MessageId,
    /// The (unique) interface ID to be used by request messages in the [USB Devices][1] interface.
    ///
    /// [1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeusb/034257d7-f7a8-4fe1-b8c2-87ac8dc4f50e
    pub usb_device: InterfaceId,
    pub device_instance_id: Cch32String,
    pub hw_ids: Option<MultiSzString>,
    pub compat_ids: Option<MultiSzString>,
    pub container_id: Cch32String,
    pub usb_device_caps: UsbDeviceCaps,
}

impl AddDevice {
    pub const NUM_USB_DEVICE: u32 = 0x1;

    pub fn header(&self) -> SharedMsgHeader {
        SharedMsgHeader {
            interface_id: InterfaceId::DEVICE_SINK,
            mask: Mask::StreamIdProxy,
            msg_id: self.msg_id,
            function_id: Some(FunctionId::ADD_DEVICE),
        }
    }

    pub(crate) fn decode(src: &mut ReadCursor<'_>, header: SharedMsgHeader) -> DecodeResult<Self> {
        ensure_size!(in: src, size: 4 /* NumUsbDevice */);
        let num_usb_device = src.read_u32();
        if num_usb_device != 0x1 {
            return Err(unsupported_value_err!("NumUsbDevice", format!("{num_usb_device}")));
        }

        ensure_size!(in: src, size: InterfaceId::FIXED_PART_SIZE);
        let usb_device = match src.read_u32() {
            // Real mstsc assigns UsbDevice=0 to a redirected device (verified live
            // 2026-07-06), so the reserved-interface-id range 0x0..=0x3 must NOT be
            // rejected here: the device is addressed by its own per-device DVC, so a
            // per-channel interface id of 0 is unambiguous and valid. (FreeRDP uses a
            // >=0x4 id — both must parse.) Only the StreamId-masked high range is an
            // invalid plain interface id. Extends divergence (1).
            value @ 0x0..=0x3F_FF_FF_FF => InterfaceId::try_from(value).map_err(|e|
                // Only a map_err and not expect (value clamped) cause clippy complains
                other_err!(source: e))?,
            value @ 0x40_00_00_00.. => return Err(unsupported_value_err!("UsbDevice", format!("{value}"))),
        };

        let device_instance_id = Cch32String::decode_owned(src)?;

        ensure_size!(in: src, size: 4 /* cchHwIds */);
        let hw_ids = if src.peek_u32() != 0 {
            Some(MultiSzString::decode_owned(src)?)
        } else {
            let _ = src.read_u32(); // skip cchHwIds
            None
        };

        ensure_size!(in: src, size: 4 /* cchCompatIds */);
        let compat_ids = if src.peek_u32() != 0 {
            Some(MultiSzString::decode_owned(src)?)
        } else {
            let _ = src.read_u32(); // skip cchCompatIds
            None
        };

        let container_id = Cch32String::decode_owned(src)?;
        let usb_device_caps = UsbDeviceCaps::decode(src)?;

        Ok(Self {
            msg_id: header.msg_id,
            usb_device,
            device_instance_id,
            hw_ids,
            compat_ids,
            container_id,
            usb_device_caps,
        })
    }
}

impl Encode for AddDevice {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_size!(in: dst, size: self.size());

        self.header().encode(dst)?;

        dst.write_u32(Self::NUM_USB_DEVICE);
        dst.write_u32(self.usb_device.into());
        self.device_instance_id.encode(dst)?;
        match &self.hw_ids {
            Some(ids) => ids.encode(dst)?,
            None => dst.write_u32(0x0),
        };
        match &self.compat_ids {
            Some(ids) => ids.encode(dst)?,
            None => dst.write_u32(0x0),
        };
        self.container_id.encode(dst)?;
        self.usb_device_caps.encode(dst)?;

        Ok(())
    }

    fn name(&self) -> &'static str {
        "ADD_DEVICE"
    }

    fn size(&self) -> usize {
        let device_instance_id = self.device_instance_id.size();
        let hw_ids = match &self.hw_ids {
            Some(hardware_ids) => hardware_ids.size(),
            None => const { size_of::<u32>() }, // cchHwIds
        };
        let compat_ids = match &self.compat_ids {
            Some(compatibility_ids) => compatibility_ids.size(),
            None => const { size_of::<u32>() }, // cchCompatIds
        };
        let container_id = self.container_id.size();

        strict_sum(&[SharedMsgHeader::SIZE_REQ
            + 4 // NumUsbDevice
            + InterfaceId::FIXED_PART_SIZE // UsbDevice
            + device_instance_id
            + hw_ids
            + compat_ids
            + container_id
            + UsbDeviceCaps::FIXED_PART_SIZE])
    }
}

/// [\[MS-RDPEUSB\] 2.2.11 USB_DEVICE_CAPABILITIES][1] packet.
///
/// Defines the capabilities of a USB device.
///
/// [1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeusb/98d4650e-b6d8-47e5-b71b-4d320ab542ee
#[doc(alias = "USB_DEVICE_CAPABILITIES")]
#[derive(Debug, PartialEq)]
pub struct UsbDeviceCaps {
    pub usb_bus_iface_ver: UsbBusIfaceVer,
    pub usbdi_ver: UsbdiVer,
    pub supported_usb_ver: SupportedUsbVer,
    pub device_speed: DeviceSpeed,
    pub no_ack_isoch_write_jitter_buf_size: NoAckIsochWriteJitterBufSizeInMs,
}

impl UsbDeviceCaps {
    pub const CB_SIZE: u32 = 28;

    pub const HCD_CAPS: u32 = 0;

    #[expect(clippy::as_conversions)]
    pub const FIXED_PART_SIZE: usize = Self::CB_SIZE as usize;
}

impl Encode for UsbDeviceCaps {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ensure_fixed_part_size!(in: dst);

        dst.write_u32(Self::CB_SIZE);

        dst.write_u32(self.usb_bus_iface_ver.to_u32());
        dst.write_u32(self.usbdi_ver.to_u32());
        dst.write_u32(self.supported_usb_ver.to_u32());

        dst.write_u32(Self::HCD_CAPS);

        dst.write_u32(self.device_speed.to_u32());

        dst.write_u32(self.no_ack_isoch_write_jitter_buf_size.0);

        Ok(())
    }

    fn name(&self) -> &'static str {
        "USB_DEVICE_CAPABILITIES"
    }

    fn size(&self) -> usize {
        Self::FIXED_PART_SIZE
    }
}

impl Decode<'_> for UsbDeviceCaps {
    fn decode(src: &mut ReadCursor<'_>) -> DecodeResult<Self> {
        ensure_fixed_part_size!(in: src);

        let cb_size = src.read_u32();
        if cb_size != Self::CB_SIZE {
            return Err(unsupported_value_err!("CbSize", format!("{cb_size}")));
        }
        // (divergence) These four fields are *device-reported* version/speed
        // values that grow over time (USB 3.x, SuperSpeed, newer bus interface
        // revisions). Upstream rejects any unnamed value, which tears down the
        // whole channel for a modern device (a USB-3 SSD sends SupportedUsbVersion
        // 0x320). Decode them leniently — unknown values are preserved verbatim in
        // an `Other` variant so the PDU still parses and the descriptors surface.
        let usb_bus_iface_ver = UsbBusIfaceVer::from_u32(src.read_u32());
        let usbdi_ver = UsbdiVer::from_u32(src.read_u32());
        let supported_usb_ver = SupportedUsbVer::from_u32(src.read_u32());
        let hcd_caps = src.read_u32();
        if hcd_caps != Self::HCD_CAPS {
            return Err(unsupported_value_err!("HcdCapabilities", format!("{hcd_caps}")));
        }
        let device_speed = DeviceSpeed::from_u32(src.read_u32());
        // 0 = isoch not supported; any other value is the jitter-buffer size in ms
        // (spec range 10..=512, but preserved verbatim rather than rejected).
        let no_ack_isoch_write_jitter_buf_size = NoAckIsochWriteJitterBufSizeInMs(src.read_u32());

        Ok(Self {
            usb_bus_iface_ver,
            usbdi_ver,
            supported_usb_ver,
            device_speed,
            no_ack_isoch_write_jitter_buf_size,
        })
    }
}

/// USB bus interface version. `Other` preserves an unnamed value verbatim (see
/// the lenient-decode divergence in `UsbDeviceCaps::decode`).
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum UsbBusIfaceVer {
    V0,
    V1,
    V2,
    Other(u32),
}

impl UsbBusIfaceVer {
    pub fn from_u32(value: u32) -> Self {
        match value {
            0x0 => Self::V0,
            0x1 => Self::V1,
            0x2 => Self::V2,
            other => Self::Other(other),
        }
    }

    pub fn to_u32(self) -> u32 {
        match self {
            Self::V0 => 0x0,
            Self::V1 => 0x1,
            Self::V2 => 0x2,
            Self::Other(v) => v,
        }
    }
}

/// USBDI version. `Other` preserves an unnamed value verbatim.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum UsbdiVer {
    V0x500,
    V0x600,
    Other(u32),
}

impl UsbdiVer {
    pub fn from_u32(value: u32) -> Self {
        match value {
            0x500 => Self::V0x500,
            0x600 => Self::V0x600,
            other => Self::Other(other),
        }
    }

    pub fn to_u32(self) -> u32 {
        match self {
            Self::V0x500 => 0x500,
            Self::V0x600 => 0x600,
            Self::Other(v) => v,
        }
    }
}

/// Highest USB version the device supports (`bcdUSB`). Extended past upstream's
/// USB 2.0 with the USB 3.x versions; `Other` preserves any future value.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum SupportedUsbVer {
    Usb10,
    Usb11,
    Usb20,
    Usb30,
    Usb31,
    Usb32,
    Other(u32),
}

impl SupportedUsbVer {
    pub fn from_u32(value: u32) -> Self {
        match value {
            0x100 => Self::Usb10,
            0x110 => Self::Usb11,
            0x200 => Self::Usb20,
            0x300 => Self::Usb30,
            0x310 => Self::Usb31,
            0x320 => Self::Usb32,
            other => Self::Other(other),
        }
    }

    pub fn to_u32(self) -> u32 {
        match self {
            Self::Usb10 => 0x100,
            Self::Usb11 => 0x110,
            Self::Usb20 => 0x200,
            Self::Usb30 => 0x300,
            Self::Usb31 => 0x310,
            Self::Usb32 => 0x320,
            Self::Other(v) => v,
        }
    }
}

/// Device speed. `Other` preserves a value the enum doesn't name (e.g. a
/// SuperSpeed encoding a USB-3 device may report).
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum DeviceSpeed {
    FullSpeed,
    HighSpeed,
    Other(u32),
}

impl DeviceSpeed {
    pub fn from_u32(value: u32) -> Self {
        match value {
            0x0 => Self::FullSpeed,
            0x1 => Self::HighSpeed,
            other => Self::Other(other),
        }
    }

    pub fn to_u32(self) -> u32 {
        match self {
            Self::FullSpeed => 0x0,
            Self::HighSpeed => 0x1,
            Self::Other(v) => v,
        }
    }
}

#[repr(transparent)]
#[derive(Debug, PartialEq, Clone, Copy)]
pub struct NoAckIsochWriteJitterBufSizeInMs(u32);

impl NoAckIsochWriteJitterBufSizeInMs {
    const TS_URB_ISOCH_TRANSFER_NOT_SUPPORTED: Self = Self(0);

    pub fn outstanding_isoch_data(&self) -> Option<u32> {
        (self.0 != 0).then_some(self.0)
    }
}

impl TryFrom<u32> for NoAckIsochWriteJitterBufSizeInMs {
    type Error = &'static str;
    // type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::TS_URB_ISOCH_TRANSFER_NOT_SUPPORTED),
            10..=512 => Ok(Self(value)),
            _ => Err("is not in the range: [10, 512]"),
        }
    }
}
