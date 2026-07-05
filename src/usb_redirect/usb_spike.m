// Phase-2 spike for generic USB redirection (MS-RDPEUSB): present a *synthetic*
// USB device to macOS via the PUBLIC IOUSBHost.framework user-space host
// controller (`IOUSBHostControllerInterface`, entitlement
// com.apple.developer.usb.host-controller-interface).
//
// Phase 1b proved the controller instantiates in a signed+provisioned build.
// Phase 2 drives the full UserHCI command protocol so the kernel enumerates a
// device:
//   2a  controller + port state machines — bring the controller Active, power
//       the root port, assert connect → kernel issues PortReset.
//   2b  DeviceCreate → device state machine + EP0 endpoint; answer the kernel's
//       GET_DESCRIPTOR control transfers with a hardcoded vendor-specific
//       device so it shows up in `ioreg -p IOUSB` / `system_profiler
//       SPUSBDataType`.
//
// The device here is HARDCODED (VID 0x1209 / PID 0x0001, pid.codes test id) —
// Phase 3 replaces the descriptor bytes + transfer handling with the real
// device redirected over MS-RDPEUSB from the RDP client. This stays the
// maintenance boundary for the Apple USB SPI: every IOUSBHost touch lives here
// (mirrors src/virtual_display/private_api.rs). Built only on macOS (build.rs
// compiles it + links IOUSBHost); driven from src/usb_redirect/mod.rs behind the
// default-OFF `--usb-spike` flag.
//
// The command + doorbell handlers run on the interface's own serial dispatch
// queue, so the mutable device/endpoint maps need no locking.

#import <Foundation/Foundation.h>
#import <IOUSBHost/IOUSBHostControllerInterface.h>
#import <IOUSBHost/IOUSBHostCIControllerStateMachine.h>
#import <IOUSBHost/IOUSBHostCIPortStateMachine.h>
#import <IOUSBHost/IOUSBHostCIDeviceStateMachine.h>
#import <IOUSBHost/IOUSBHostCIEndpointStateMachine.h>
#import <mach/mach_time.h>

// ---- helpers to pull a bit-field out of an IOUSBHostCIMessage word ----------
// The SDK phase macros expand to the low bit index; mask is (range) already
// shifted, so extract = (word & range) >> phase.
static inline uint32_t msg_type(const IOUSBHostCIMessage *m) {
    return (uint32_t)((m->control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
}
static inline BOOL msg_valid(const IOUSBHostCIMessage *m) {
    return (m->control & IOUSBHostCIMessageControlValid) != 0;
}
static inline BOOL msg_no_response(const IOUSBHostCIMessage *m) {
    return (m->control & IOUSBHostCIMessageControlNoResponse) != 0;
}

// ---- hardcoded synthetic device descriptors ---------------------------------
// Full-speed vendor-specific device with only the default control endpoint.
// Full-speed so the host never asks for a device_qualifier (which we'd stall).
static const uint8_t kDeviceDescriptor[] = {
    0x12,       // bLength
    0x01,       // bDescriptorType = DEVICE
    0x00, 0x02, // bcdUSB 2.00
    0xFF,       // bDeviceClass = vendor-specific
    0x00,       // bDeviceSubClass
    0x00,       // bDeviceProtocol
    0x40,       // bMaxPacketSize0 = 64
    0x09, 0x12, // idVendor  = 0x1209 (pid.codes)
    0x01, 0x00, // idProduct = 0x0001 (test)
    0x00, 0x01, // bcdDevice 1.00
    0x01,       // iManufacturer -> string 1
    0x02,       // iProduct      -> string 2
    0x00,       // iSerialNumber -> none
    0x01,       // bNumConfigurations
};

static const uint8_t kConfigDescriptor[] = {
    // Configuration descriptor
    0x09,       // bLength
    0x02,       // bDescriptorType = CONFIGURATION
    0x12, 0x00, // wTotalLength = 18
    0x01,       // bNumInterfaces
    0x01,       // bConfigurationValue
    0x00,       // iConfiguration
    0x80,       // bmAttributes = bus-powered
    0x32,       // bMaxPower = 100 mA
    // Interface descriptor (control-only, no data endpoints)
    0x09,       // bLength
    0x04,       // bDescriptorType = INTERFACE
    0x00,       // bInterfaceNumber
    0x00,       // bAlternateSetting
    0x00,       // bNumEndpoints (EP0 only)
    0xFF,       // bInterfaceClass = vendor-specific
    0x00,       // bInterfaceSubClass
    0x00,       // bInterfaceProtocol
    0x00,       // iInterface
};

static const uint8_t kStringLangIDs[] = { 0x04, 0x03, 0x09, 0x04 }; // 0x0409 en-US
static const uint8_t kStringManufacturer[] = {
    0x0E, 0x03, 'm', 0, 'a', 0, 'c', 0, 'r', 0, 'd', 0, 'p', 0,
};
static const uint8_t kStringProduct[] = {
    0x16, 0x03, 'm', 0, 'a', 0, 'c', 0, 'r', 0, 'd', 0, 'p', 0,
    ' ', 0, 'U', 0, 'S', 0, 'B', 0,
};

// USB standard request / descriptor-type constants.
enum {
    kUsbDirDeviceToHost   = 0x80,
    kUsbReqGetDescriptor  = 0x06,
    kUsbReqSetAddress     = 0x05,
    kUsbReqSetConfig      = 0x09,
    kUsbDescDevice        = 0x01,
    kUsbDescConfiguration = 0x02,
    kUsbDescString        = 0x03,
};

static const NSUInteger kSyntheticDeviceAddress = 1;

// -----------------------------------------------------------------------------

@interface MacrdpUsbController : NSObject
@property(nonatomic, strong) IOUSBHostControllerInterface *interface;
// deviceAddress -> device state machine
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, IOUSBHostCIDeviceStateMachine *> *devices;
// (deviceAddress<<8 | endpointAddress) -> endpoint state machine
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, IOUSBHostCIEndpointStateMachine *> *endpoints;
// endpoint key -> pending 8-byte SETUP packet for the in-flight control transfer
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, NSData *> *pendingSetup;
@property(nonatomic, assign) BOOL portConnected;
@end

@implementation MacrdpUsbController

- (instancetype)init {
    if ((self = [super init])) {
        _devices = [NSMutableDictionary dictionary];
        _endpoints = [NSMutableDictionary dictionary];
        _pendingSetup = [NSMutableDictionary dictionary];
    }
    return self;
}

// Bring the root port to "connected" once the controller is Active and the port
// is powered. Writing the SM's `connected` property auto-sends a PortEvent with
// ConnectChange, which makes the kernel issue a PortReset next.
- (void)maybeConnectPort {
    if (self.portConnected) {
        return;
    }
    NSError *e = nil;
    IOUSBHostCIControllerStateMachine *csm = self.interface.controllerStateMachine;
    if (csm.controllerState != IOUSBHostCIControllerStateActive) {
        return;
    }
    IOUSBHostCIPortStateMachine *port = [self.interface getPortStateMachineForPort:1 error:&e];
    if (port == nil) {
        NSLog(@"[usb2] getPortStateMachineForPort:1 failed: %@", e);
        return;
    }
    if (!port.powered) {
        return;
    }
    port.connected = YES;
    self.portConnected = YES;
    NSLog(@"[usb2] root port connect asserted — expecting PortReset from kernel");
}

- (void)handleControllerCommand:(const IOUSBHostCIMessage *)command type:(uint32_t)type {
    NSError *e = nil;
    IOUSBHostCIControllerStateMachine *csm = self.interface.controllerStateMachine;
    if (![csm inspectCommand:command error:&e]) {
        NSLog(@"[usb2] controller inspectCommand rejected type=0x%02x: %@", type, e);
        return;
    }
    if (type == IOUSBHostCIMessageTypeControllerFrameNumber) {
        // Provide a synthetic 1ms frame counter derived from mach time.
        static mach_timebase_info_data_t tb;
        if (tb.denom == 0) {
            mach_timebase_info(&tb);
        }
        uint64_t now = mach_absolute_time();
        uint64_t ns = now * tb.numer / tb.denom;
        uint64_t frame = ns / 1000000ull; // 1ms frames
        [csm respondToCommand:command status:IOUSBHostCIMessageStatusSuccess frame:frame timestamp:now error:&e];
    } else {
        [csm respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
    }
    if (e) {
        NSLog(@"[usb2] controller respond type=0x%02x error: %@", type, e);
    }
    NSLog(@"[usb2] controller cmd type=0x%02x -> state=%ld", type, (long)csm.controllerState);
    [self maybeConnectPort];
}

- (void)handlePortCommand:(const IOUSBHostCIMessage *)command type:(uint32_t)type {
    NSError *e = nil;
    IOUSBHostCIPortStateMachine *port = [self.interface getPortStateMachineForCommand:command error:&e];
    if (port == nil) {
        NSLog(@"[usb2] getPortStateMachineForCommand type=0x%02x failed: %@", type, e);
        return;
    }
    if (![port inspectCommand:command error:&e]) {
        NSLog(@"[usb2] port inspectCommand rejected type=0x%02x: %@", type, e);
        return;
    }
    switch (type) {
        case IOUSBHostCIMessageTypePortPowerOn:
            port.powered = YES;
            break;
        case IOUSBHostCIMessageTypePortPowerOff:
            port.powered = NO;
            self.portConnected = NO;
            break;
        case IOUSBHostCIMessageTypePortReset:
            // Reset drives the link to U0 (operational) at our device speed.
            // Full-speed avoids the host asking for a device_qualifier.
            if (![port updateLinkState:IOUSBHostCILinkStateU0
                                 speed:IOUSBHostCIDeviceSpeedFull
                inhibitLinkStateChange:NO
                                 error:&e]) {
                NSLog(@"[usb2] updateLinkState on reset failed: %@", e);
            }
            break;
        default:
            break; // Resume/Suspend/Disable/Status: just ack.
    }
    [port respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
    if (e) {
        NSLog(@"[usb2] port respond type=0x%02x error: %@", type, e);
    }
    NSLog(@"[usb2] port cmd type=0x%02x -> state=%ld powered=%d connected=%d",
          type, (long)port.portState, port.powered, port.connected);
    [self maybeConnectPort];
}

- (void)handleDeviceCommand:(const IOUSBHostCIMessage *)command type:(uint32_t)type {
    NSError *e = nil;
    if (type == IOUSBHostCIMessageTypeDeviceCreate) {
        IOUSBHostCIDeviceStateMachine *dev =
            [[IOUSBHostCIDeviceStateMachine alloc] initWithInterface:self.interface command:command error:&e];
        if (dev == nil) {
            NSLog(@"[usb2] DeviceCreate state machine init failed: %@", e);
            return;
        }
        [dev respondToCommand:command
                       status:IOUSBHostCIMessageStatusSuccess
                deviceAddress:kSyntheticDeviceAddress
                        error:&e];
        if (e) {
            NSLog(@"[usb2] DeviceCreate respond error: %@", e);
            return;
        }
        self.devices[@(kSyntheticDeviceAddress)] = dev;
        NSLog(@"[usb2] device created addr=%lu route=%lu",
              (unsigned long)dev.deviceAddress, (unsigned long)dev.completeRoute);
        return;
    }

    // Non-create device command: target existing device by address in data0.
    NSUInteger addr = (command->data0 & IOUSBHostCICommandMessageData0DeviceAddress)
                    >> IOUSBHostCICommandMessageData0DeviceAddressPhase;
    IOUSBHostCIDeviceStateMachine *dev = self.devices[@(addr)];
    if (dev == nil) {
        NSLog(@"[usb2] device cmd type=0x%02x for unknown addr=%lu", type, (unsigned long)addr);
        return;
    }
    if (![dev inspectCommand:command error:&e]) {
        NSLog(@"[usb2] device inspectCommand rejected type=0x%02x: %@", type, e);
        return;
    }
    [dev respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
    if (type == IOUSBHostCIMessageTypeDeviceDestroy) {
        [self.devices removeObjectForKey:@(addr)];
    }
    NSLog(@"[usb2] device cmd type=0x%02x addr=%lu state=%ld",
          type, (unsigned long)addr, (long)dev.deviceState);
}

- (NSNumber *)endpointKeyForDevice:(NSUInteger)dev endpoint:(NSUInteger)ep {
    return @((dev << 8) | (ep & 0xff));
}

- (void)handleEndpointCommand:(const IOUSBHostCIMessage *)command type:(uint32_t)type {
    NSError *e = nil;
    NSUInteger addr = (command->data0 & IOUSBHostCICommandMessageData0DeviceAddress)
                    >> IOUSBHostCICommandMessageData0DeviceAddressPhase;
    NSUInteger epAddr = (command->data0 & IOUSBHostCICommandMessageData0EndpointAddress)
                      >> IOUSBHostCICommandMessageData0EndpointAddressPhase;

    if (type == IOUSBHostCIMessageTypeEndpointCreate) {
        IOUSBHostCIEndpointStateMachine *ep =
            [[IOUSBHostCIEndpointStateMachine alloc] initWithInterface:self.interface command:command error:&e];
        if (ep == nil) {
            NSLog(@"[usb2] EndpointCreate state machine init failed: %@", e);
            return;
        }
        [ep respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
        if (e) {
            NSLog(@"[usb2] EndpointCreate respond error: %@", e);
            return;
        }
        NSNumber *key = [self endpointKeyForDevice:ep.deviceAddress endpoint:ep.endpointAddress];
        self.endpoints[key] = ep;
        NSLog(@"[usb2] endpoint created dev=%lu ep=0x%02lx state=%ld",
              (unsigned long)ep.deviceAddress, (unsigned long)ep.endpointAddress, (long)ep.endpointState);
        return;
    }

    NSNumber *key = [self endpointKeyForDevice:addr endpoint:epAddr];
    IOUSBHostCIEndpointStateMachine *ep = self.endpoints[key];
    if (ep == nil) {
        NSLog(@"[usb2] endpoint cmd type=0x%02x for unknown dev=%lu ep=0x%02lx",
              type, (unsigned long)addr, (unsigned long)epAddr);
        return;
    }
    if (![ep inspectCommand:command error:&e]) {
        NSLog(@"[usb2] endpoint inspectCommand rejected type=0x%02x: %@", type, e);
        return;
    }
    [ep respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
    if (type == IOUSBHostCIMessageTypeEndpointDestroy) {
        [self.endpoints removeObjectForKey:key];
        [self.pendingSetup removeObjectForKey:key];
    }
    NSLog(@"[usb2] endpoint cmd type=0x%02x dev=%lu ep=0x%02lx state=%ld",
          type, (unsigned long)addr, (unsigned long)epAddr, (long)ep.endpointState);
}

- (void)handleCommand:(const IOUSBHostCIMessage *)command {
    uint32_t type = msg_type(command);
    if (type >= IOUSBHostCIMessageTypeControllerPowerOn && type <= IOUSBHostCIMessageTypeControllerFrameNumber) {
        [self handleControllerCommand:command type:type];
    } else if (type >= IOUSBHostCIMessageTypePortPowerOn && type <= IOUSBHostCIMessageTypePortStatus) {
        [self handlePortCommand:command type:type];
    } else if (type >= IOUSBHostCIMessageTypeDeviceCreate && type <= IOUSBHostCIMessageTypeDeviceUpdate) {
        [self handleDeviceCommand:command type:type];
    } else if (type >= IOUSBHostCIMessageTypeEndpointCreate && type <= IOUSBHostCIMessageTypeEndpointSetNextTransfer) {
        [self handleEndpointCommand:command type:type];
    } else {
        NSLog(@"[usb2] unhandled command type=0x%02x control=0x%08x", type, command->control);
    }
}

// ---- control-transfer (EP0) handling ----------------------------------------

// Build the response bytes for a GET_DESCRIPTOR request. Returns the descriptor
// and its length via out-params; returns NO to STALL an unsupported request.
- (BOOL)descriptorForValue:(uint16_t)wValue index:(uint16_t)wIndex
                     bytes:(const uint8_t **)outBytes length:(size_t *)outLen {
    uint8_t descType = (wValue >> 8) & 0xff;
    uint8_t descIndex = wValue & 0xff;
    switch (descType) {
        case kUsbDescDevice:
            *outBytes = kDeviceDescriptor;
            *outLen = sizeof(kDeviceDescriptor);
            return YES;
        case kUsbDescConfiguration:
            *outBytes = kConfigDescriptor;
            *outLen = sizeof(kConfigDescriptor);
            return YES;
        case kUsbDescString:
            switch (descIndex) {
                case 0: *outBytes = kStringLangIDs;      *outLen = sizeof(kStringLangIDs);      return YES;
                case 1: *outBytes = kStringManufacturer; *outLen = sizeof(kStringManufacturer); return YES;
                case 2: *outBytes = kStringProduct;      *outLen = sizeof(kStringProduct);      return YES;
                default: return NO;
            }
        default:
            return NO; // device_qualifier, BOS, etc. — stall.
    }
}

// Process one transfer message on EP0; return the status to complete it with and
// the number of bytes moved. Interprets SETUP/DATA/STATUS stages of a control
// transfer, currently only servicing GET_DESCRIPTOR (everything else is ACKed
// with zero-length so enumeration's SET_ADDRESS / SET_CONFIGURATION succeed).
- (IOUSBHostCIMessageStatus)processTransfer:(const IOUSBHostCIMessage *)msg
                                  endpointKey:(NSNumber *)key
                                transferLength:(NSUInteger *)outLen {
    *outLen = 0;
    uint32_t type = msg_type(msg);
    switch (type) {
        case IOUSBHostCIMessageTypeSetupTransfer: {
            // data1 packs the 8-byte setup packet. Stash it for the data stage.
            uint64_t d1 = msg->data1;
            uint8_t setup[8];
            setup[0] = (d1 >> IOUSBHostCISetupTransferData1bmRequestTypePhase) & 0xff;
            setup[1] = (d1 >> IOUSBHostCISetupTransferData1bRequestPhase) & 0xff;
            uint16_t wValue = (d1 >> IOUSBHostCISetupTransferData1wValuePhase) & 0xffff;
            uint16_t wIndex = (d1 >> IOUSBHostCISetupTransferData1wIndexPhase) & 0xffff;
            uint16_t wLength = (d1 >> IOUSBHostCISetupTransferData1wLengthPhase) & 0xffff;
            setup[2] = wValue & 0xff;       setup[3] = wValue >> 8;
            setup[4] = wIndex & 0xff;       setup[5] = wIndex >> 8;
            setup[6] = wLength & 0xff;      setup[7] = wLength >> 8;
            self.pendingSetup[key] = [NSData dataWithBytes:setup length:8];
            NSLog(@"[usb2] EP0 SETUP bmReq=0x%02x bReq=0x%02x wValue=0x%04x wIndex=0x%04x wLength=%u",
                  setup[0], setup[1], wValue, wIndex, wLength);
            return IOUSBHostCIMessageStatusSuccess;
        }
        case IOUSBHostCIMessageTypeNormalTransfer: {
            // Control-transfer data stage. Buffer VA is in our address space.
            NSUInteger bufLen = (msg->data0 & IOUSBHostCINormalTransferData0Length)
                              >> IOUSBHostCINormalTransferData0LengthPhase;
            void *buf = (void *)(uintptr_t)msg->data1;
            NSData *setupData = self.pendingSetup[key];
            if (setupData == nil || buf == NULL) {
                return IOUSBHostCIMessageStatusSuccess; // nothing to move
            }
            const uint8_t *s = setupData.bytes;
            uint8_t bmRequestType = s[0];
            uint8_t bRequest = s[1];
            uint16_t wValue = (uint16_t)s[2] | ((uint16_t)s[3] << 8);
            uint16_t wIndex = (uint16_t)s[4] | ((uint16_t)s[5] << 8);
            if ((bmRequestType & kUsbDirDeviceToHost) && bRequest == kUsbReqGetDescriptor) {
                const uint8_t *desc = NULL; size_t descLen = 0;
                if (![self descriptorForValue:wValue index:wIndex bytes:&desc length:&descLen]) {
                    NSLog(@"[usb2] EP0 GET_DESCRIPTOR unsupported wValue=0x%04x -> STALL", wValue);
                    return IOUSBHostCIMessageStatusStallError;
                }
                size_t n = descLen < bufLen ? descLen : bufLen;
                memcpy(buf, desc, n);
                *outLen = n;
                NSLog(@"[usb2] EP0 IN data stage moved %zu bytes (wValue=0x%04x)", n, wValue);
                return IOUSBHostCIMessageStatusSuccess;
            }
            // Host-to-device or unhandled IN: accept without moving data.
            return IOUSBHostCIMessageStatusSuccess;
        }
        case IOUSBHostCIMessageTypeStatusTransfer:
            [self.pendingSetup removeObjectForKey:key];
            return IOUSBHostCIMessageStatusSuccess;
        default:
            NSLog(@"[usb2] unexpected transfer type=0x%02x on EP0", type);
            return IOUSBHostCIMessageStatusSuccess;
    }
}

- (void)handleDoorbell:(IOUSBHostCIDoorbell)doorbell {
    NSUInteger addr = (doorbell & IOUSBHostCIDoorbellDeviceAddress) >> IOUSBHostCIDoorbellDeviceAddressPhase;
    NSUInteger epAddr = (doorbell & IOUSBHostCIDoorbellEndpointAddress) >> IOUSBHostCIDoorbellEndpointAddressPhase;
    NSNumber *key = [self endpointKeyForDevice:addr endpoint:epAddr];
    IOUSBHostCIEndpointStateMachine *ep = self.endpoints[key];
    if (ep == nil) {
        NSLog(@"[usb2] doorbell for unknown dev=%lu ep=0x%02lx", (unsigned long)addr, (unsigned long)epAddr);
        return;
    }
    NSError *e = nil;
    if (![ep processDoorbell:doorbell error:&e]) {
        NSLog(@"[usb2] processDoorbell dev=%lu ep=0x%02lx failed: %@", (unsigned long)addr, (unsigned long)epAddr, e);
        return;
    }
    // Walk the transfer ring: the SM follows Link messages and advances
    // currentTransferMessage as each is completed. Guard against a stuck ring.
    for (int guard = 0; guard < 64; guard++) {
        if (ep.endpointState != IOUSBHostCIEndpointStateActive) {
            break;
        }
        const IOUSBHostCIMessage *msg = ep.currentTransferMessage;
        if (msg == NULL || !msg_valid(msg)) {
            break;
        }
        NSUInteger moved = 0;
        IOUSBHostCIMessageStatus status = [self processTransfer:msg endpointKey:key transferLength:&moved];
        if (msg_no_response(msg)) {
            // No completion expected; without an advance API this would loop —
            // control transfers always want a response, so treat as done.
            NSLog(@"[usb2] EP0 transfer with NoResponse set — stopping ring walk");
            break;
        }
        if (![ep enqueueTransferCompletionForMessage:msg status:status transferLength:moved error:&e]) {
            NSLog(@"[usb2] enqueueTransferCompletion failed: %@", e);
            break;
        }
    }
}

- (BOOL)startWithError:(NSError **)error {
    // Controller capabilities: one root port.
    IOUSBHostCIMessage controllerCaps = {
        .control = (IOUSBHostCIMessageTypeControllerCapabilities << IOUSBHostCIMessageControlTypePhase)
                 | IOUSBHostCIMessageControlNoResponse
                 | IOUSBHostCIMessageControlValid
                 | (1u << IOUSBHostCICapabilitiesMessageControlPortCountPhase),
        .data0   = (1u << IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase)
                 | (2u << IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase),
        .data1   = 0,
    };
    // Port capabilities: port #1, ACPI Type-A connector.
    IOUSBHostCIMessage portCaps = {
        .control = (IOUSBHostCIMessageTypePortCapabilities << IOUSBHostCIMessageControlTypePhase)
                 | IOUSBHostCIMessageControlNoResponse
                 | IOUSBHostCIMessageControlValid
                 | (1u << IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase)
                 | (0u << IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase),
        .data0   = ((907 / 8) << IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase),
        .data1   = 0,
    };
    NSMutableData *caps = [[NSMutableData alloc] initWithBytes:&controllerCaps length:sizeof(IOUSBHostCIMessage)];
    [caps appendBytes:&portCaps length:sizeof(IOUSBHostCIMessage)];

    __weak MacrdpUsbController *weakSelf = self;
    IOUSBHostControllerInterface *iface =
        [[IOUSBHostControllerInterface alloc]
            initWithCapabilities:caps
                           queue:nil
                 interruptRateHz:1000
                           error:error
                  commandHandler:^(IOUSBHostControllerInterface *c, IOUSBHostCIMessage command) {
                      (void)c;
                      [weakSelf handleCommand:&command];
                  }
                 doorbellHandler:^(IOUSBHostControllerInterface *c, IOUSBHostCIDoorbell *doorbells, uint32_t count) {
                      (void)c;
                      for (uint32_t i = 0; i < count; i++) {
                          [weakSelf handleDoorbell:doorbells[i]];
                      }
                  }
                 interestHandler:NULL];
    if (iface == nil) {
        return NO;
    }
    self.interface = iface;
    return YES;
}

- (void)stop {
    [self.interface destroy];
    self.interface = nil;
}

@end

// Returns 0 if the controller was created and the enumeration loop ran (see the
// logs for how far the kernel drove us / whether the device appeared), non-zero
// on init failure.
int macrdp_usb_spike_run(void) {
    @autoreleasepool {
        MacrdpUsbController *controller = [[MacrdpUsbController alloc] init];
        NSError *error = nil;
        if (![controller startWithError:&error] || (error != nil && error.code != KERN_SUCCESS)) {
            NSLog(@"[usb2] NO-GO: IOUSBHostControllerInterface init failed: %@", error);
            return error != nil ? (int)error.code : -1;
        }
        NSLog(@"[usb2] controller created — driving enumeration. Watch for the "
              @"synthetic device (VID 0x1209/PID 0x0001) in `ioreg -p IOUSB` / "
              @"`system_profiler SPUSBDataType`. Holding 20s...");
        [NSThread sleepForTimeInterval:20.0];
        [controller stop];
        NSLog(@"[usb2] controller destroyed; exiting");
        return 0;
    }
}
