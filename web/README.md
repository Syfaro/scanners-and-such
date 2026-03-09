# scanners-and-such-web

A WASM build of scanners-and-such for handling advanced barcode scanner features
and decoding scanned data.

## Examples

### SNAPI Device

```typescript
import { SnapiDevice } from "@syfaro/scanners-and-such-web";

const devices = await navigator.hid.requestDevice({
    filters: [{ vendorId: 0x05E0, productId: 0x1900 }]
});

if (devices) {
    const device = new SnapiDevice();

    await device.start(devices[0], (output) => {
        console.log(output);
    });

    const serialNumber = (await device.getAttribute(534)).value.trim();
    console.log(serialNumber);
}
```
