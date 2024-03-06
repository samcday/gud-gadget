use std::time::Duration;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing::{debug, trace, warn};
use usb_gadget::Class;
use usb_gadget::function::custom::{CtrlSender, Custom, Endpoint, EndpointDirection, EndpointReceiver, Interface};
use usb_gadget::function::{custom, Handle};
use bytes::BytesMut;

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

const GUD_DISPLAY_FLAG_FULL_UPDATE: u32 = 0x02;

const GUD_CONNECTOR_STATUS_CONNECTED: u8 = 0x01;

const GUD_PIXEL_FORMAT_RGB565: u8 = 0x40;
const GUD_PIXEL_FORMAT_RGB888: u8 = 0x50;
const GUD_PIXEL_FORMAT_XRGB8888: u8 = 0x80;

const GUD_CONNECTOR_TYPE_PANEL: u8 = 0;

const GUD_STATUS_OK: u8 = 0;

const GUD_COMPRESSION_LZ4: u8 = 0x01;

#[derive(Serialize)]
struct ConnectorDescriptor {
    connector_type: u8,
    flags: u32,
}

pub struct Function {
    ep0: Custom,
}

pub struct PixelDataEndpoint {
    ep_rx: EndpointReceiver,
    ep_buf: Vec<BytesMut>,
}

#[derive(Debug, Serialize)]
pub struct DisplayMode {
    pub clock: u32,
    pub hdisplay: u16,
    pub htotal: u16,
    pub hsync_end: u16,
    pub hsync_start: u16,
    pub vtotal: u16,
    pub vdisplay: u16,
    pub vsync_end: u16,
    pub vsync_start: u16,
    pub flags: u32
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

#[derive(Debug)]
pub enum Event<'a> {
    GetDescriptorRequest(GetDescriptorRequest<'a>),
    GetDisplayModesRequest(GetDisplayModesRequest<'a>),
    Buffer(SetBuffer),
}

#[derive(Debug)]
pub struct GetDescriptorRequest<'a> {
    sender: CtrlSender<'a>
}

#[derive(Debug)]
pub struct GetDisplayModesRequest<'a> {
    sender: CtrlSender<'a>,
}

impl<'a> GetDescriptorRequest<'a> {
    pub fn send_descriptor(self, min_width: u32, min_height: u32, max_width: u32, max_height: u32) -> anyhow::Result<()> {
        let descriptor = DisplayDescriptor {
            magic: GUD_DISPLAY_MAGIC,
            version: 1,
            flags: 0,
            compression: 0,
            // compression: GUD_COMPRESSION_LZ4,
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

impl<'a> GetDisplayModesRequest<'a> {
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

impl Function {
    pub fn new() -> (Self, PixelDataEndpoint, Handle) {
        let (ep_rx, ep1_dir) = EndpointDirection::host_to_device();
        let (ep0, handle) = Custom::builder()
            .with_interface(Interface::new(Class::vendor_specific(0, 0), "GUD")
                .with_endpoint(Endpoint::bulk(ep1_dir)))
            .build();

        (Self {
            ep0,
        }, PixelDataEndpoint {
            ep_rx,
            ep_buf: Vec::new(),
        }, handle)
    }

    pub fn event(&mut self, timeout: Duration) -> anyhow::Result<Option<Event>> {
        if let Some(event) = self.ep0.event_timeout(timeout)? {
            trace!("received event {:?}", event);
            match event {
                custom::Event::Enable => {},
                custom::Event::Bind => {},
                custom::Event::SetupDeviceToHost(req) => {
                    let ctrl_req = req.ctrl_req();
                    match ctrl_req.request {
                        GUD_REQ_GET_STATUS => {
                            req.send(&[GUD_STATUS_OK]).context("send status")?;
                            debug!("sent status");
                        }
                        GUD_REQ_GET_DESCRIPTOR => {
                            return Ok(Some(Event::GetDescriptorRequest(GetDescriptorRequest { sender: req })));
                        }
                        GUD_REQ_GET_FORMATS => {
                            req.send(&[
                                // GUD_PIXEL_FORMAT_XRGB8888,
                                GUD_PIXEL_FORMAT_RGB565,
                            ]).context("send pixel formats")?;
                            debug!("sent pixel formats");
                        }
                        GUD_REQ_GET_PROPERTIES => {
                            let sent = req.send(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).context("send properties")?;
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
                            req.send(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).context("send connector properties")?;
                            debug!("sent connector properties");
                        }
                        GUD_REQ_GET_CONNECTOR_MODES => {
                            return Ok(Some(Event::GetDisplayModesRequest(GetDisplayModesRequest { sender: req })));
                        }
                        GUD_REQ_GET_CONNECTOR_EDID => {
                            req.send(&[0]).context("send EDIDs")?;
                            debug!("sent EDIDs");
                        }
                        GUD_REQ_GET_CONNECTOR_STATUS => {
                            req.send(&[GUD_CONNECTOR_STATUS_CONNECTED]).context("send connector status")?;
                            debug!("sent connector status");
                        }
                        req => {
                            warn!("unhandled SetupDeviceToHost request {:x}", req);
                        }
                    }
                },
                custom::Event::SetupHostToDevice(req) => {
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
                            (v, _) = ssmarshal::deserialize(req.as_slice()).context("deserialize set buffer")?;
                            debug!("received set buffer: {:?}", v);
                            return Ok(Some(Event::Buffer(v)))
                        }
                        v => {
                            warn!("unhandled set request {:x}", v);
                        },
                    }
                },
                event => {
                    warn!("unhandled event {:?}", event);
                }
            }
        }
        Ok(None)
    }
}

impl PixelDataEndpoint {
    pub fn recv_buffer(&mut self, info: SetBuffer, mut fb: &mut [u8], fb_pitch: usize) -> anyhow::Result<()> {
        let mut remaining = info.length as usize;
        let max_packet_size = self.ep_rx.max_packet_size().unwrap();
        // TODO: use pixel format provided in state check
        let pixel_size = (info.length / info.width / info.height) as usize;

        // Advance framebuffer ptr to starting position.
        fb = &mut fb[fb_pitch * info.y as usize..];

        // Calculate starting position (in bytes) for each line.
        let line_offset = pixel_size * info.x as usize;
        // Total width of a line (in bytes).
        let line_width = pixel_size * info.width as usize;
        // Set up a slice for current line of pixels (this is what we'll copy to).
        let mut line = &mut fb[line_offset..(line_offset + line_width)];

        while remaining > 0 {
            let buf = self.ep_buf.pop().unwrap_or_else(|| BytesMut::with_capacity(max_packet_size));
            let buf = self.ep_rx.recv(buf).context("read bulk ep")?;
            if buf.is_none() {
                continue;
            }
            let buf = buf.unwrap();
            let mut data = buf.as_ref();
            remaining -= data.len();

            while data.len() > 0 {
                if line.len() == 0 {
                    // Advance to the next line in the framebuffer.
                    fb = &mut fb[fb_pitch..];
                    // Update line slice to new position in fb.
                    line = &mut fb[line_offset..(line_offset + line_width)];
                }

                let src = &data[0..std::cmp::min(line.len(), data.len())];

                // Do the copy.
                (&mut line[0..src.len()]).copy_from_slice(src);

                // Advance the position in current line slice, and in incoming data slice.
                data = &data[src.len()..];
                line = &mut line[src.len()..];
            }

            self.ep_buf.push(buf);
        }

        Ok(())
    }
}
