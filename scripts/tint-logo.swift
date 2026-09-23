import Foundation
import CoreGraphics
import CoreImage
import ImageIO
import UniformTypeIdentifiers
import AppKit

// Fill CQ's letterforms with its three tool colours — Paster's green,
// Shotter's blue, Jotter's orange — softly blended on a transparent
// background. This is where the logo comes from; the PNGs in `public/` are its
// output and are regenerated rather than edited.
//
//   swift scripts/tint-logo.swift <in.png> <out.png> [blur]
//
// The source is the plain white letterforms, whose alpha is the mask: only the
// shape is taken, and the colour is painted through it. To remake the shipped
// set from a white master at `master.png`:
//
//   for f in logo-white logo-black logo; do
//     swift scripts/tint-logo.swift master.png "public/$f.png" 0.05
//   done
//   swift scripts/tint-logo.swift master.png src-tauri/icons/tray-colour.png 0.05
//
// `logo-white` and `logo-black` are the same image: the colours carry on light
// and dark alike, so the theme swap has nothing left to swap. Both files stay
// so the markup referring to them needs no change.
//
// 0.05 is the blend the logo ships with — tight enough that the three colours
// stay distinct at size. Larger numbers melt them together.

let args = CommandLine.arguments
guard args.count >= 3,
      let src = CGImageSourceCreateWithURL(URL(fileURLWithPath: args[1]) as CFURL, nil),
      let logo = CGImageSourceCreateImageAtIndex(src, 0, nil) else {
    print("cannot read input"); exit(1)
}
let w = logo.width, h = logo.height
let blurFrac = args.count > 3 ? Double(args[3]) ?? 0.10 : 0.10

// The three pools, positioned as they are in the working blob: green upper
// left, blue upper right, orange low centre.
let pools: [(CGPoint, CGColor, Double)] = [
    (CGPoint(x: 0.30 * Double(w), y: 0.62 * Double(h)),
     CGColor(red: 38/255, green: 194/255, blue: 168/255, alpha: 1), 0.62),
    (CGPoint(x: 0.70 * Double(w), y: 0.58 * Double(h)),
     CGColor(red: 56/255, green: 152/255, blue: 232/255, alpha: 1), 0.62),
    (CGPoint(x: 0.50 * Double(w), y: 0.28 * Double(h)),
     CGColor(red: 240/255, green: 122/255, blue: 56/255, alpha: 1), 0.60),
]

let space = CGColorSpaceCreateDeviceRGB()
guard let paint = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8,
                            bytesPerRow: 0, space: space,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { exit(1) }
// An opaque bed of the middle colour so the pools have something to melt into
// and there are no dead corners inside a thick letterform.
paint.setFillColor(CGColor(red: 60/255, green: 168/255, blue: 200/255, alpha: 1))
paint.fill(CGRect(x: 0, y: 0, width: w, height: h))
for (centre, colour, radiusFrac) in pools {
    let r = radiusFrac * Double(max(w, h))
    guard let grad = CGGradient(colorsSpace: space,
                                colors: [colour, colour.copy(alpha: 0)!] as CFArray,
                                locations: [0, 1]) else { continue }
    paint.drawRadialGradient(grad, startCenter: centre, startRadius: 0,
                             endCenter: centre, endRadius: r, options: [])
}
guard let painted = paint.makeImage() else { exit(1) }

// Blur it, so the colours have no seams — the same reason the blob is blurred.
let ci = CIImage(cgImage: painted)
let blurred: CGImage
if let f = CIFilter(name: "CIGaussianBlur") {
    f.setValue(ci, forKey: kCIInputImageKey)
    f.setValue(blurFrac * Double(max(w, h)), forKey: kCIInputRadiusKey)
    let ctx = CIContext()
    // Clamp first, or the blur pulls transparency in from outside the edges.
    let clamped = ci.clampedToExtent()
    f.setValue(clamped, forKey: kCIInputImageKey)
    blurred = ctx.createCGImage(f.outputImage!, from: ci.extent) ?? painted
} else {
    blurred = painted
}

// Paint it through the logo's own shape.
guard let out = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8,
                          bytesPerRow: 0, space: space,
                          bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { exit(1) }
let rect = CGRect(x: 0, y: 0, width: w, height: h)
out.clip(to: rect, mask: logo)
out.draw(blurred, in: rect)
guard let result = out.makeImage(),
      let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: args[2]) as CFURL,
                                                 UTType.png.identifier as CFString, 1, nil) else { exit(1) }
CGImageDestinationAddImage(dest, result, nil)
CGImageDestinationFinalize(dest)
print("wrote \(args[2]) (\(w)x\(h))")
