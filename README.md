# gud-gadget

It's a gadget. And it is GUD.

## wut?

[GUD is an open USB display protocol.](https://github.com/notro/gud/wiki) The host driver has been available in [mainline Linux kernels since 5.13](https://github.com/torvalds/linux/tree/v5.13/drivers/gpu/drm/gud). The host *sends* display output to a USB device.

A device-side implementation exists as a Linux kernel module, but it has not been mainlined, and (as of writing) does not build on the latest 6.x kernel releases.

The [`gud-gadget`](./gud-gadget) crate implements the device side in user-space with [FunctionFS](https://docs.kernel.org/usb/functionfs.html).

The [`gud-gadget-drm`](./gud-gadget-drm) crate is a basic implementation of the gud-gadget that uses drm for display.
