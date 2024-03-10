# gud-gadget

It's a gadget. And it is GUD.

## wut?

[GUD is an open USB display protocol.](https://github.com/notro/gud/wiki) The host driver has been available in [mainline Linux kernels since 5.13](https://github.com/torvalds/linux/tree/v5.13/drivers/gpu/drm/gud). The host *sends* display output to a USB device.

An official [gadget implementation](https://github.com/notro/gud/wiki/Linux-Gadget-Driver) exists as a Linux kernel module, but it has not been mainlined, and (as of writing) does not build on the latest 6.x kernel releases.

The [`gud-function`](./gadget) crate implements a GUD gadget as a [FunctionFS](https://docs.kernel.org/usb/functionfs.html) function, for use with the [usb-gadget](https://crates.io/crates/usb-gadget) crate.

The [`gud-drm`](./drm) crate is a simple implementation that configures a GUD gadget with the `gud-function` implementation, and renders the pixel data directly to a [drm](https://en.wikipedia.org/wiki/Direct_Rendering_Manager) framebuffer.
