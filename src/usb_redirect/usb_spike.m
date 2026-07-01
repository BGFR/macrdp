// Phase-1b go/no-go spike for generic USB redirection (MS-RDPEUSB).
//
// The single question this answers: with the granted entitlement
// com.apple.developer.usb.host-controller-interface present in a signed +
// provisioned build, can we instantiate a user-space USB host controller via
// the PUBLIC IOUSBHost.framework API `IOUSBHostControllerInterface`? That class
// is the documented path to "create synthetic USB devices" (its header) — the
// presenting side macrdp needs. If the alloc/init succeeds, the route is real
// and Phases 2–3 (present a device + wire MS-RDPEUSB) are unblocked; if it
// fails (entitlement rejected / SPI unavailable), it's a NO-GO.
//
// This is the maintenance boundary for the private-ish Apple USB SPI: all
// IOUSBHost touches live here (mirrors src/virtual_display/private_api.rs).
// Built only on macOS (build.rs compiles it + links IOUSBHost); called from
// src/usb_redirect/mod.rs behind the default-OFF `--usb-spike` flag.
//
// Capabilities layout mirrors the worked example in Apple's
// IOUSBHostControllerInterface.h (one root port, ACPI Type-A).

#import <Foundation/Foundation.h>
#import <IOUSBHost/IOUSBHostControllerInterface.h>

// Returns 0 if the controller was created (entitlement honored, route works),
// non-zero otherwise (the NSError code, or -1).
int macrdp_usb_spike_run(void) {
    @autoreleasepool {
        // Controller capabilities: advertise a single root port.
        IOUSBHostCIMessage controllerCaps = {
            .control = (IOUSBHostCIMessageTypeControllerCapabilities << IOUSBHostCIMessageControlTypePhase)
                     | IOUSBHostCIMessageControlNoResponse
                     | IOUSBHostCIMessageControlValid
                     | (1u << IOUSBHostCICapabilitiesMessageControlPortCountPhase),        // 1 port
            .data0   = (1u << IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase)  // 2^1 = 2s
                     | (2u << IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase),        // 2^2 = 4ms
            .data1   = 0
        };

        // Port capabilities: port #1, ACPI Type-A connector.
        IOUSBHostCIMessage portCaps = {
            .control = (IOUSBHostCIMessageTypePortCapabilities << IOUSBHostCIMessageControlTypePhase)
                     | IOUSBHostCIMessageControlNoResponse
                     | IOUSBHostCIMessageControlValid
                     | (1u << IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase)     // port 1
                     | (0u << IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase), // ACPI TypeA
            .data0   = ((907 / 8) << IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase), // max power (8mA units)
            .data1   = 0
        };

        NSMutableData *capabilities = [[NSMutableData alloc] initWithBytes:&controllerCaps
                                                                    length:sizeof(IOUSBHostCIMessage)];
        [capabilities appendBytes:&portCaps length:sizeof(IOUSBHostCIMessage)];

        NSError *error = nil;

        IOUSBHostControllerInterface *controller =
            [[IOUSBHostControllerInterface alloc]
                initWithCapabilities:capabilities
                               queue:nil
                     interruptRateHz:1000
                               error:&error
                      commandHandler:^(IOUSBHostControllerInterface *c, IOUSBHostCIMessage command) {
                          // Minimal: just observe. A full implementation would drive the
                          // controller/port/device/endpoint state machines here.
                          (void)c;
                          NSLog(@"[usb-spike] command control=0x%08x data0=0x%08x", command.control, command.data0);
                      }
                     doorbellHandler:^(IOUSBHostControllerInterface *c, IOUSBHostCIDoorbell *doorbells, uint32_t count) {
                          (void)c; (void)doorbells;
                          NSLog(@"[usb-spike] doorbell count=%u", count);
                      }
                     interestHandler:NULL];

        if (controller == nil || (error != nil && error.code != KERN_SUCCESS)) {
            NSLog(@"[usb-spike] NO-GO: IOUSBHostControllerInterface init failed: %@", error);
            return error != nil ? (int)error.code : -1;
        }

        NSLog(@"[usb-spike] GO: IOUSBHostControllerInterface created — entitlement honored, UserHCI route works. "
              @"Holding 8s so the controller is visible in `ioreg -p IOUSB` / `system_profiler SPUSBDataType`...");
        [NSThread sleepForTimeInterval:8.0];

        [controller destroy];
        NSLog(@"[usb-spike] controller destroyed; exiting cleanly");
        return 0;
    }
}
