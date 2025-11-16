use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read, Write};
use tracing::{debug, warn};

use usb_gadget::function::custom::{Endpoint, EndpointDirection};
use usb_gadget::Id;

const GUD_DISPLAY_MAGIC: u32 = 0x1d50614d;

const GUD_REQ_GET_STATUS: u8 = 0x00;
const GUD_REQ_GET_DESCRIPTOR: u8 = 0x01;
const GUD_REQ_GET_FORMATS: u8 = 0x40;
const GUD_REQ_GET_PROPERTIES: u8 = 0x41;
const GUD_REQ_GET_CONNECTORS: u8 = 0x50;
const GUD_REQ_GET_CONNECTOR_PROPERTIES: u8 = 0x51;
const GUD_REQ_GET_CONNECTOR_STATUS: u8 = 0x54;
const GUD_REQ_GET_CONNECTOR_MODES: u8 = 0x55;
const GUD_REQ_GET_CONNECTOR_EDID: u8 = 0x56;

const GUD_REQ_SET_CONNECTOR_FORCE_DETECT: u8 = 0x53;
const GUD_REQ_SET_BUFFER: u8 = 0x60;
const GUD_REQ_SET_STATE_CHECK: u8 = 0x61;
const GUD_REQ_SET_STATE_COMMIT: u8 = 0x62;
const GUD_REQ_SET_CONTROLLER_ENABLE: u8 = 0x63;
const GUD_REQ_SET_DISPLAY_ENABLE: u8 = 0x64;

const GUD_CONNECTOR_STATUS_CONNECTED: u8 = 0x01;

pub const GUD_PIXEL_FORMAT_RGB565: u8 = 0x40;
pub const GUD_PIXEL_FORMAT_RGB888: u8 = 0x50;
pub const GUD_PIXEL_FORMAT_XRGB8888: u8 = 0x80;

const GUD_CONNECTOR_TYPE_PANEL: u8 = 0;

const GUD_STATUS_OK: u8 = 0;

const GUD_COMPRESSION_LZ4: u8 = 0x01;

// https://github.com/openmoko/openmoko-usb-oui/commit/73bdf541b6f9840b70219626b4088d4e3f164904
pub const OPENMOKO_GUD_ID: Id = Id::new(0x1d50, 0x614d);

#[derive(Serialize)]
struct ConnectorDescriptor {
    connector_type: u8,
    flags: u32,
}

#[derive(Debug, Serialize)]
pub struct DisplayMode {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub flags: u32,
}

#[derive(Deserialize, Debug)]
pub struct SetBuffer {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub length: u32,
    pub compression: u8,
    pub compressed_length: u32,
}

const FUNCTIONFS_EVENT_SIZE: usize = 12;
const USB_DIR_IN: u8 = 0x80;

#[derive(Debug, Clone, Copy)]
pub struct CtrlReq {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl CtrlReq {
    fn parse(data: &[u8]) -> io::Result<Self> {
        if data.len() < 8 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short control request",
            ));
        }
        Ok(Self {
            request_type: data[0],
            request: data[1],
            value: u16::from_le_bytes([data[2], data[3]]),
            index: u16::from_le_bytes([data[4], data[5]]),
            length: u16::from_le_bytes([data[6], data[7]]),
        })
    }
}

#[derive(Debug)]
pub enum FunctionfsEvent<'a> {
    Bind,
    Unbind,
    Enable,
    Disable,
    Suspend,
    Resume,
    SetupDeviceToHost(CtrlSender<'a>),
    SetupHostToDevice(CtrlReceiver<'a>),
    Unknown(u8),
}

pub struct CtrlSender<'a> {
    ctrl_req: CtrlReq,
    ep0: &'a mut File,
    handled: bool,
}

impl<'a> std::fmt::Debug for CtrlSender<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CtrlSender")
            .field("ctrl_req", &self.ctrl_req)
            .finish()
    }
}

impl<'a> CtrlSender<'a> {
    fn new(ctrl_req: CtrlReq, ep0: &'a mut File) -> Self {
        Self {
            ctrl_req,
            ep0,
            handled: false,
        }
    }

    pub fn ctrl_req(&self) -> &CtrlReq {
        &self.ctrl_req
    }

    pub fn len(&self) -> usize {
        self.ctrl_req.length.into()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn send(mut self, data: &[u8]) -> anyhow::Result<usize> {
        self.ep0.write_all(data).context("write ep0 response")?;
        self.handled = true;
        Ok(data.len())
    }

    pub fn halt(mut self) -> anyhow::Result<()> {
        self.do_halt()
    }

    fn do_halt(&mut self) -> anyhow::Result<()> {
        let mut buf = [0u8; 1];
        self.ep0.read(&mut buf).context("stall ep0 response")?;
        self.handled = true;
        Ok(())
    }
}

impl<'a> Drop for CtrlSender<'a> {
    fn drop(&mut self) {
        if !self.handled {
            let _ = self.do_halt();
        }
    }
}

pub struct CtrlReceiver<'a> {
    ctrl_req: CtrlReq,
    ep0: &'a mut File,
    handled: bool,
}

impl<'a> std::fmt::Debug for CtrlReceiver<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CtrlReceiver")
            .field("ctrl_req", &self.ctrl_req)
            .finish()
    }
}

impl<'a> CtrlReceiver<'a> {
    fn new(ctrl_req: CtrlReq, ep0: &'a mut File) -> Self {
        Self {
            ctrl_req,
            ep0,
            handled: false,
        }
    }

    pub fn ctrl_req(&self) -> &CtrlReq {
        &self.ctrl_req
    }

    pub fn len(&self) -> usize {
        self.ctrl_req.length.into()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn recv_all(mut self) -> anyhow::Result<Vec<u8>> {
        let mut buf = vec![0; self.len()];
        if !buf.is_empty() {
            self.ep0.read_exact(&mut buf).context("read ep0 payload")?;
        }
        self.handled = true;
        Ok(buf)
    }

    pub fn halt(mut self) -> anyhow::Result<()> {
        self.do_halt()
    }

    fn do_halt(&mut self) -> anyhow::Result<()> {
        let buf = [0u8; 1];
        self.ep0.write(&buf).context("stall ep0 request")?;
        self.handled = true;
        Ok(())
    }
}

impl<'a> Drop for CtrlReceiver<'a> {
    fn drop(&mut self) {
        if !self.handled {
            let _ = self.do_halt();
        }
    }
}

pub fn read_functionfs_event<'a>(ep0: &'a mut File) -> io::Result<FunctionfsEvent<'a>> {
    let mut buf = [0u8; FUNCTIONFS_EVENT_SIZE];
    ep0.read_exact(&mut buf)?;
    let event_type = buf[8];
    Ok(match event_type {
        0 => FunctionfsEvent::Bind,
        1 => FunctionfsEvent::Unbind,
        2 => FunctionfsEvent::Enable,
        3 => FunctionfsEvent::Disable,
        4 => {
            let ctrl_req = CtrlReq::parse(&buf[..8])?;
            if (ctrl_req.request_type & USB_DIR_IN) != 0 {
                FunctionfsEvent::SetupDeviceToHost(CtrlSender::new(ctrl_req, ep0))
            } else {
                FunctionfsEvent::SetupHostToDevice(CtrlReceiver::new(ctrl_req, ep0))
            }
        }
        5 => FunctionfsEvent::Suspend,
        6 => FunctionfsEvent::Resume,
        other => FunctionfsEvent::Unknown(other),
    })
}

#[derive(Debug)]
pub enum Event<'a> {
    GetDescriptor(GetDescriptor<'a>),
    GetDisplayModes(GetDisplayModes<'a>),
    GetPixelFormats(GetPixelFormats<'a>),
    Buffer(SetBuffer),
}

#[derive(Debug)]
pub struct GetDescriptor<'a> {
    sender: CtrlSender<'a>,
}

#[derive(Debug)]
pub struct GetDisplayModes<'a> {
    sender: CtrlSender<'a>,
}

#[derive(Debug)]
pub struct GetPixelFormats<'a> {
    sender: CtrlSender<'a>,
}

impl<'a> GetDescriptor<'a> {
    pub fn send_descriptor(
        self,
        min_width: u32,
        min_height: u32,
        max_width: u32,
        max_height: u32,
    ) -> anyhow::Result<()> {
        let descriptor = DisplayDescriptor {
            magic: GUD_DISPLAY_MAGIC,
            version: 1,
            flags: 0,
            compression: GUD_COMPRESSION_LZ4,
            max_height,
            max_width,
            min_height,
            min_width,
            max_buffer_size: max_height * max_width * 4,
        };

        let mut buf: [u8; 30] = [0; 30];
        ssmarshal::serialize(&mut buf, &descriptor).context("serialize display descriptor")?;

        self.sender.send(&buf).context("send display descriptor")?;
        debug!("sent display descriptor {:?}", descriptor);
        Ok(())
    }
}

impl<'a> GetDisplayModes<'a> {
    pub fn send_modes(self, modes: &[DisplayMode]) -> anyhow::Result<()> {
        let size = 24 * modes.len();
        if size > self.sender.len() {
            // TODO: proper Err
            panic!("too many display modes provided");
        }

        let mut buf = vec![0; size];
        let mut pos = 0;
        for mode in modes {
            pos = pos + ssmarshal::serialize(&mut buf[pos..], mode).context("serialize mode")?;
        }

        self.sender.send(&buf).context("send modes")?;

        Ok(())
    }
}

impl<'a> GetPixelFormats<'a> {
    pub fn send_pixel_formats(self, formats: &[u8]) -> anyhow::Result<()> {
        self.sender.send(formats).context("send pixel formats")?;
        debug!("sent pixel formats: {:?}", formats);
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct DisplayDescriptor {
    magic: u32,
    version: u8,
    flags: u32,
    compression: u8,
    max_buffer_size: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

pub fn event(event: FunctionfsEvent) -> anyhow::Result<Option<Event>> {
    match event {
        FunctionfsEvent::Enable => {}
        FunctionfsEvent::Bind => {}
        FunctionfsEvent::SetupDeviceToHost(req) => {
            let ctrl_req = req.ctrl_req();
            match ctrl_req.request {
                GUD_REQ_GET_STATUS => {
                    req.send(&[GUD_STATUS_OK]).context("send status")?;
                    debug!("sent status");
                }
                GUD_REQ_GET_DESCRIPTOR => {
                    return Ok(Some(Event::GetDescriptor(GetDescriptor { sender: req })));
                }
                GUD_REQ_GET_FORMATS => {
                    return Ok(Some(Event::GetPixelFormats(GetPixelFormats {
                        sender: req,
                    })));
                }
                GUD_REQ_GET_PROPERTIES => {
                    let sent = req
                        .send(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                        .context("send properties")?;
                    debug!("sent properties {}", sent);
                }
                GUD_REQ_GET_CONNECTORS => {
                    let connectors = [ConnectorDescriptor {
                        connector_type: GUD_CONNECTOR_TYPE_PANEL,
                        flags: 0,
                    }];

                    let mut buf: [u8; 5] = [0; 5];
                    ssmarshal::serialize(&mut buf, &connectors).context("serialize connectors")?;
                    req.send(&buf).context("send connectors")?;
                    debug!("sent connectors");
                }
                GUD_REQ_GET_CONNECTOR_PROPERTIES => {
                    req.send(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                        .context("send connector properties")?;
                    debug!("sent connector properties");
                }
                GUD_REQ_GET_CONNECTOR_MODES => {
                    return Ok(Some(Event::GetDisplayModes(GetDisplayModes {
                        sender: req,
                    })));
                }
                GUD_REQ_GET_CONNECTOR_EDID => {
                    req.send(&[0]).context("send EDIDs")?;
                    debug!("sent EDIDs");
                }
                GUD_REQ_GET_CONNECTOR_STATUS => {
                    req.send(&[GUD_CONNECTOR_STATUS_CONNECTED])
                        .context("send connector status")?;
                    debug!("sent connector status");
                }
                req => {
                    warn!("unhandled SetupDeviceToHost request {:x}", req);
                }
            }
        }
        FunctionfsEvent::SetupHostToDevice(req) => {
            let ctrl_req = req.ctrl_req();
            match ctrl_req.request {
                GUD_REQ_SET_CONNECTOR_FORCE_DETECT => {
                    debug!("connector set to {}", ctrl_req.value);
                    req.recv_all().context("recv set connector")?;
                }
                GUD_REQ_SET_STATE_CHECK => {
                    debug!("received state check");
                    req.recv_all().context("recv set state check")?;
                }
                GUD_REQ_SET_CONTROLLER_ENABLE => {
                    let req = req.recv_all().context("recv set controller enable")?;
                    debug!("received controller enable: {:?}", req);
                }
                GUD_REQ_SET_DISPLAY_ENABLE => {
                    let req = req.recv_all().context("recv set display enable")?;
                    debug!("received display enable: {:?}", req);
                }
                GUD_REQ_SET_STATE_COMMIT => {
                    req.recv_all().context("recv set state commit")?;
                    debug!("received state commit");
                }
                GUD_REQ_SET_BUFFER => {
                    let req = req.recv_all().context("recv set buffer")?;
                    let v: SetBuffer;
                    (v, _) =
                        ssmarshal::deserialize(req.as_slice()).context("deserialize set buffer")?;
                    debug!("received set buffer: {:?}", v);
                    return Ok(Some(Event::Buffer(v)));
                }
                v => {
                    warn!("unhandled set request {:x}", v);
                }
            }
        }
        FunctionfsEvent::Disable
        | FunctionfsEvent::Suspend
        | FunctionfsEvent::Resume
        | FunctionfsEvent::Unbind => {}
        FunctionfsEvent::Unknown(event_id) => {
            warn!("unhandled functionfs event {}", event_id);
        }
    }
    Ok(None)
}

pub fn pixel_data_endpoint() -> Endpoint {
    let (_rx, dir) = EndpointDirection::host_to_device();
    Endpoint::bulk(dir)
}
