use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::time::Instant;
use tracing::{debug, trace, warn};

use bytes::BytesMut;
use usb_gadget::function::custom;
use usb_gadget::function::custom::{CtrlSender, EndpointDirection, EndpointReceiver};
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

const GUD_DISPLAY_FLAG_FULL_UPDATE: u32 = 0x02;

const GUD_CONNECTOR_STATUS_CONNECTED: u8 = 0x01;

const GUD_PIXEL_FORMAT_RGB565: u8 = 0x40;
const GUD_PIXEL_FORMAT_RGB888: u8 = 0x50;
const GUD_PIXEL_FORMAT_XRGB8888: u8 = 0x80;

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

pub struct PixelDataEndpoint {
    ep_rx: EndpointReceiver,
    // A collection of the small buffers we've allocated for submission to AIO to read from the endpoint.
    ep_buf: Vec<BytesMut>,
    // The full contents of a transmitted buffer are copied here.
    buf: BytesMut,
    // If compression is enabled, the received buffer is decompressed here.
    compress_buf: BytesMut,
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

#[derive(Debug)]
pub enum Event<'a> {
    GetDescriptorRequest(GetDescriptorRequest<'a>),
    GetDisplayModesRequest(GetDisplayModesRequest<'a>),
    Buffer(SetBuffer),
}

#[derive(Debug)]
pub struct GetDescriptorRequest<'a> {
    sender: CtrlSender<'a>,
}

#[derive(Debug)]
pub struct GetDisplayModesRequest<'a> {
    sender: CtrlSender<'a>,
}

impl<'a> GetDescriptorRequest<'a> {
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

pub fn event(event: custom::Event) -> anyhow::Result<Option<Event>> {
    match event {
        custom::Event::Enable => {}
        custom::Event::Bind => {}
        custom::Event::SetupDeviceToHost(req) => {
            let ctrl_req = req.ctrl_req();
            match ctrl_req.request {
                GUD_REQ_GET_STATUS => {
                    req.send(&[GUD_STATUS_OK]).context("send status")?;
                    debug!("sent status");
                }
                GUD_REQ_GET_DESCRIPTOR => {
                    return Ok(Some(Event::GetDescriptorRequest(GetDescriptorRequest {
                        sender: req,
                    })));
                }
                GUD_REQ_GET_FORMATS => {
                    req.send(&[
                        GUD_PIXEL_FORMAT_XRGB8888,
                        // GUD_PIXEL_FORMAT_RGB565,
                    ])
                    .context("send pixel formats")?;
                    debug!("sent pixel formats");
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
                    return Ok(Some(Event::GetDisplayModesRequest(
                        GetDisplayModesRequest { sender: req },
                    )));
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
        event => {
            warn!("unhandled event {:?}", event);
        }
    }
    Ok(None)
}

impl PixelDataEndpoint {
    pub fn new() -> (Self, EndpointDirection) {
        let (ep_rx, ep_dir) = EndpointDirection::host_to_device();

        (
            Self {
                ep_rx,
                ep_buf: Vec::new(),
                buf: BytesMut::new(),
                compress_buf: BytesMut::new(),
            },
            ep_dir,
        )
    }

    pub fn recv_buffer(
        &mut self,
        info: SetBuffer,
        fb: &mut [u8],
        fb_pitch: usize,
    ) -> anyhow::Result<()> {
        let start = Instant::now();
        let max_packet_size = self.ep_rx.max_packet_size().unwrap();
        // TODO: use pixel format provided in state check
        let bpp = (info.length / info.width / info.height) as usize;

        let len = if info.compression > 0 {
            info.compressed_length
        } else {
            info.length
        } as usize;
        self.buf.clear();

        // Ensure the buffer is large enough to fit all incoming data.
        if self.buf.capacity() < len {
            self.buf.reserve(len - self.buf.capacity());
        }

        // Read the incoming data fully into the buffer.
        let read_start = Instant::now();
        while self.buf.len() < len {
            let buf = self
                .ep_buf
                .pop()
                .unwrap_or_else(|| BytesMut::with_capacity(max_packet_size));
            let buf = self.ep_rx.recv(buf).context("read bulk ep")?;
            if buf.is_none() {
                continue;
            }
            let mut buf = buf.unwrap();
            self.buf.extend_from_slice(&buf);
            buf.clear();
            self.ep_buf.push(buf);
        }
        trace!("read buffer took {}ms", read_start.elapsed().as_millis());

        if self.buf.len() != len {
            // TODO: proper Err
            panic!("expected buf len {}, got {}", len, self.buf.len());
        }

        let buf = if info.compression > 0 {
            let decompress_start = Instant::now();
            if self.compress_buf.len() < info.length as usize {
                self.compress_buf
                    .resize(info.length as usize - self.compress_buf.capacity(), 0);
            }
            lz4::block::decompress_to_buffer(
                &self.buf,
                Some(info.length as i32),
                &mut self.compress_buf,
            )
            .context("lz4 decompress")?;
            trace!(
                "decompress buffer took {}ms",
                decompress_start.elapsed().as_millis()
            );
            &self.compress_buf
        } else {
            &self.buf
        };

        let mut y = info.y as usize;
        let end_y = (info.y + info.height) as usize;

        let line_len = info.width as usize * bpp;
        let line_start = info.x as usize * bpp;

        let mut buf_pos = 0usize;
        while y < end_y {
            let fb_start = (y * fb_pitch) + line_start;
            let fb_end = fb_start + line_len;
            fb[fb_start..fb_end].copy_from_slice(&buf[buf_pos..buf_pos + line_len]);
            buf_pos += line_len;
            y += 1;
        }

        trace!("recv_buffer took {}ms", start.elapsed().as_millis());

        Ok(())
    }
}
