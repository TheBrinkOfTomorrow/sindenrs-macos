// Phase 0 macOS check: capture 180 frames from the Sinden camera at 640x480 420v through
// AVFoundation, report the frame rate, and save the last luma plane as frame.pgm.
// Run from Terminal.app (it needs camera permission): swiftc -O camera_grab.swift -o grab && ./grab

import AVFoundation
import Foundation
let sem = DispatchSemaphore(value: 0)
AVCaptureDevice.requestAccess(for: .video) { ok in print("access:", ok); sem.signal() }
sem.wait()
let dev = AVCaptureDevice.DiscoverySession(deviceTypes: [.external], mediaType: .video, position: .unspecified).devices.first { $0.localizedName.hasPrefix("Sinden") }!
let fmt = dev.formats.first { f in
  let d = CMVideoFormatDescriptionGetDimensions(f.formatDescription)
  return d.width == 640 && d.height == 480 && CMFormatDescriptionGetMediaSubType(f.formatDescription) == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
}!
try dev.lockForConfiguration()
dev.activeFormat = fmt
dev.activeVideoMinFrameDuration = fmt.videoSupportedFrameRateRanges[0].minFrameDuration
dev.activeVideoMaxFrameDuration = fmt.videoSupportedFrameRateRanges[0].minFrameDuration
dev.unlockForConfiguration()
final class D: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
  var n = 0; var t0 = Date(); let done = DispatchSemaphore(value: 0)
  func captureOutput(_ o: AVCaptureOutput, didOutput sb: CMSampleBuffer, from c: AVCaptureConnection) {
    n += 1
    if n == 1 { t0 = Date() }
    if n == 180 {
      let pb = CMSampleBufferGetImageBuffer(sb)!
      CVPixelBufferLockBaseAddress(pb, .readOnly)
      let w = CVPixelBufferGetWidthOfPlane(pb, 0), h = CVPixelBufferGetHeightOfPlane(pb, 0), bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, 0)
      let base = CVPixelBufferGetBaseAddressOfPlane(pb, 0)!.assumingMemoryBound(to: UInt8.self)
      var out = Data("P5\n\(w) \(h)\n255\n".utf8); var sum = 0
      for y in 0..<h { for x in 0..<w { let v = base[y*bpr+x]; out.append(v); sum += Int(v) } }
      CVPixelBufferUnlockBaseAddress(pb, .readOnly)
      try! out.write(to: URL(fileURLWithPath: "frame.pgm"))
      let dt = Date().timeIntervalSince(t0)
      print("frames: \(n) in \(String(format: "%.2f", dt))s = \(String(format: "%.1f", Double(n-1)/dt)) fps, \(w)x\(h) bpr=\(bpr) luma mean=\(sum/(w*h))")
      done.signal()
    }
  }
}
let s = AVCaptureSession()
s.addInput(try AVCaptureDeviceInput(device: dev))
let o = AVCaptureVideoDataOutput()
o.videoSettings = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange]
o.alwaysDiscardsLateVideoFrames = true
let d = D(); o.setSampleBufferDelegate(d, queue: DispatchQueue(label: "cap"))
s.addOutput(o)
s.startRunning()
if d.done.wait(timeout: .now() + 20) == .timedOut { print("timeout, frames:", d.n) }
s.stopRunning()
