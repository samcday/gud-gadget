# gud-gadget-drm

This is a somewhat naive binary that exists primarily to test/demonstrate the [gud-gadget](`../gud-gadget`) implementation.

It takes full control of a drm card and writes framebuffer data there.

## postmarketOS usage example

Chances are your host machine (presumably a desktop/laptop PC) is more powerful than the postmarketOS device you're testing with.

You can build this crate for pmOS targets using [cross]():

```
# Run these commands from the top level gud-gadget directory.

# For arm64 devices
cross build --release --target aarch64-unknown-linux-musl

# For armv7 devices (untested)
cross build --release --target armv7-unknown-linux-musleabihf
```

You can then copy the statically linked binary from `./target/aarch64-unknown-linux-musl/release/gud-gadget-drm` to the target device.

Running it from the device is simple:

```
# Stop the graphical desktop.
service tinydm stop

# Run the gadget
./gud-gadget-drm /dev/dri/card0 # or whichever card is available on your device
```
