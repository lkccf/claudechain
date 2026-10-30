// 生成 1024×1024 的 App 图标：本机 → 中转 → 住宅（带房子的橙色节点）
import AppKit

let size = 1024
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
                           samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                           bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

func rgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat, _ a: CGFloat = 1) -> CGColor {
    NSColor(srgbRed: r / 255, green: g / 255, blue: b / 255, alpha: a).cgColor
}

// macOS 图标网格：824×824 内容区，圆角 185
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
let shape = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)

ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 30, color: rgb(0, 0, 0, 0.35))
ctx.addPath(shape)
ctx.setFillColor(rgb(16, 20, 34))
ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(shape)
ctx.clip()
let bg = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(),
                    colors: [rgb(44, 54, 88), rgb(16, 20, 36)] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(bg, start: CGPoint(x: 300, y: 924), end: CGPoint(x: 724, y: 100), options: [])

let a = CGPoint(x: 282, y: 420), b = CGPoint(x: 512, y: 640), c = CGPoint(x: 742, y: 420)

ctx.setLineCap(.round)
ctx.setLineJoin(.round)
ctx.setLineWidth(30)
ctx.setStrokeColor(rgb(255, 255, 255, 0.45))
ctx.move(to: a)
ctx.addLine(to: b)
ctx.addLine(to: c)
ctx.strokePath()

func dot(_ p: CGPoint, _ r: CGFloat, _ color: CGColor) {
    ctx.setFillColor(color)
    ctx.fillEllipse(in: CGRect(x: p.x - r, y: p.y - r, width: r * 2, height: r * 2))
}
dot(a, 58, rgb(240, 244, 255))
dot(b, 58, rgb(118, 196, 250))

ctx.saveGState()
ctx.setShadow(offset: .zero, blur: 70, color: rgb(240, 140, 90, 0.85))
dot(c, 96, rgb(236, 132, 84))
ctx.restoreGState()

let conf = NSImage.SymbolConfiguration(pointSize: 92, weight: .bold).applying(.init(paletteColors: [.white]))
if let house = NSImage(systemSymbolName: "house.fill", accessibilityDescription: nil)?.withSymbolConfiguration(conf) {
    let s = house.size
    house.draw(in: CGRect(x: c.x - s.width / 2, y: c.y - s.height / 2 + 4, width: s.width, height: s.height))
}
ctx.restoreGState()

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
