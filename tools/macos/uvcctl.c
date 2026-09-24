// Phase 1 macOS spike: read and set UVC camera controls on the Sinden camera with plain USB
// control requests, alongside the system UVC driver (no device open, no seize).
//
//   clang -framework IOKit -framework CoreFoundation uvcctl.c -o uvcctl
//   ./uvcctl                  dump descriptors and current exposure / brightness / contrast
//   ./uvcctl exposure 78      manual exposure, 100 us units
//   ./uvcctl exposure auto    back to aperture priority (auto)
//   ./uvcctl contrast 50      likewise brightness, gain

#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOCFPlugIn.h>
#include <IOKit/IOKitLib.h>
#include <IOKit/usb/IOUSBLib.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CAM_VID 0x32e4
#define CAM_PID 0x9210

// UVC 1.1: requests, entity subtypes, selectors.
enum { SET_CUR = 0x01, GET_CUR = 0x81, GET_MIN, GET_MAX, GET_RES, GET_DEF = 0x87 };
enum { VC_INPUT_TERMINAL = 0x02, VC_PROCESSING_UNIT = 0x05 };
enum { CT_AE_MODE = 0x02, CT_EXPOSURE_TIME_ABSOLUTE = 0x04 };
enum { PU_BRIGHTNESS = 0x02, PU_CONTRAST = 0x03, PU_GAIN = 0x04 };

static IOUSBDeviceInterface650 **dev;
static int vc_if = -1, camera_id = -1, pu_id = -1;

static IOReturn request(int in, int req, int sel, int entity, void *buf, int len) {
    IOUSBDevRequest r = {
        .bmRequestType = in ? 0xA1 : 0x21,
        .bRequest = req,
        .wValue = sel << 8,
        .wIndex = (entity << 8) | vc_if,
        .wLength = len,
        .pData = buf,
    };
    return (*dev)->DeviceRequest(dev, &r);
}

static long get(int req, int sel, int entity, int len, int sign) {
    unsigned char b[4] = {0};
    IOReturn kr = request(1, req, sel, entity, b, len);
    if (kr != kIOReturnSuccess) return -999999;
    long v = b[0] | b[1] << 8 | (long)b[2] << 16 | (long)b[3] << 24;
    if (sign && len == 2) v = (short)v;
    return v;
}

static void show(const char *name, int sel, int entity, int len, int sign) {
    printf("%-10s cur=%ld min=%ld max=%ld res=%ld def=%ld\n", name,
           get(GET_CUR, sel, entity, len, sign), get(GET_MIN, sel, entity, len, sign),
           get(GET_MAX, sel, entity, len, sign), get(GET_RES, sel, entity, len, sign),
           get(GET_DEF, sel, entity, len, sign));
}

static int set(int sel, int entity, int len, long v) {
    unsigned char b[4] = {v & 0xff, v >> 8 & 0xff, v >> 16 & 0xff, v >> 24 & 0xff};
    IOReturn kr = request(0, SET_CUR, sel, entity, b, len);
    if (kr != kIOReturnSuccess) {
        fprintf(stderr, "SET_CUR sel %#x entity %d failed: %#x\n", sel, entity, kr);
        return 1;
    }
    return 0;
}

static int open_camera(void) {
    CFMutableDictionaryRef match = IOServiceMatching(kIOUSBDeviceClassName);
    int vid = CAM_VID, pid = CAM_PID;
    CFDictionarySetValue(match, CFSTR(kUSBVendorID),
                         CFNumberCreate(NULL, kCFNumberIntType, &vid));
    CFDictionarySetValue(match, CFSTR(kUSBProductID),
                         CFNumberCreate(NULL, kCFNumberIntType, &pid));
    io_service_t svc = IOServiceGetMatchingService(kIOMainPortDefault, match);
    if (!svc) {
        fprintf(stderr, "no %04x:%04x camera\n", CAM_VID, CAM_PID);
        return 1;
    }
    IOCFPlugInInterface **plug;
    SInt32 score;
    IOReturn kr = IOCreatePlugInInterfaceForService(svc, kIOUSBDeviceUserClientTypeID,
                                                    kIOCFPlugInInterfaceID, &plug, &score);
    IOObjectRelease(svc);
    if (kr || !plug) {
        fprintf(stderr, "plug-in: %#x\n", kr);
        return 1;
    }
    (*plug)->QueryInterface(plug, CFUUIDGetUUIDBytes(kIOUSBDeviceInterfaceID650),
                            (LPVOID *)&dev);
    (*plug)->Release(plug);
    if (!dev) {
        fprintf(stderr, "no device interface\n");
        return 1;
    }
    return 0;
}

// Walk the configuration descriptor for the VideoControl interface and its entity IDs.
static void parse(int verbose) {
    IOUSBConfigurationDescriptorPtr cfg;
    if ((*dev)->GetConfigurationDescriptorPtr(dev, 0, &cfg)) {
        fprintf(stderr, "no configuration descriptor\n");
        exit(1);
    }
    const unsigned char *p = (const unsigned char *)cfg, *end = p + cfg->wTotalLength;
    int cur_if = -1, cur_class = -1, cur_sub = -1;
    for (; p + 2 <= end && p[0]; p += p[0]) {
        if (p[1] == 0x04) {  // INTERFACE
            cur_if = p[2], cur_class = p[5], cur_sub = p[6];
            if (verbose)
                printf("interface %d alt %d class %d subclass %d\n", p[2], p[3], p[5], p[6]);
            if (cur_class == 14 && cur_sub == 1) vc_if = cur_if;
        } else if (p[1] == 0x24 && cur_class == 14 && cur_sub == 1) {  // CS_INTERFACE, VC
            if (p[2] == VC_INPUT_TERMINAL && (p[4] | p[5] << 8) == 0x0201) {
                camera_id = p[3];
                if (verbose)
                    printf("  camera terminal id %d bmControls %02x %02x %02x\n", p[3],
                           p[15], p[16], p[17]);
            } else if (p[2] == VC_PROCESSING_UNIT) {
                pu_id = p[3];
                if (verbose)
                    printf("  processing unit id %d bmControls %02x %02x\n", p[3], p[8], p[9]);
            }
        }
    }
    if (vc_if < 0 || camera_id < 0 || pu_id < 0) {
        fprintf(stderr, "VideoControl if %d camera %d pu %d: incomplete\n", vc_if, camera_id,
                pu_id);
        exit(1);
    }
}

int main(int argc, char **argv) {
    if (open_camera()) return 1;
    parse(argc < 2);
    if (argc < 3) {
        printf("VideoControl interface %d, camera terminal %d, processing unit %d\n", vc_if,
               camera_id, pu_id);
        show("ae_mode", CT_AE_MODE, camera_id, 1, 0);
        show("exposure", CT_EXPOSURE_TIME_ABSOLUTE, camera_id, 4, 0);
        show("brightness", PU_BRIGHTNESS, pu_id, 2, 1);
        show("contrast", PU_CONTRAST, pu_id, 2, 0);
        show("gain", PU_GAIN, pu_id, 2, 0);
        return 0;
    }
    const char *what = argv[1], *val = argv[2];
    if (!strcmp(what, "exposure")) {
        if (!strcmp(val, "auto")) return set(CT_AE_MODE, camera_id, 1, 8);
        return set(CT_AE_MODE, camera_id, 1, 1) ||
               set(CT_EXPOSURE_TIME_ABSOLUTE, camera_id, 4, atol(val));
    }
    if (!strcmp(what, "brightness")) return set(PU_BRIGHTNESS, pu_id, 2, atol(val));
    if (!strcmp(what, "contrast")) return set(PU_CONTRAST, pu_id, 2, atol(val));
    if (!strcmp(what, "gain")) return set(PU_GAIN, pu_id, 2, atol(val));
    fprintf(stderr, "unknown control %s\n", what);
    return 1;
}
