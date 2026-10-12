import AppKit
import Darwin

private struct Command: Decodable {
  let op: String
  let trial: UInt64?
  let text: String?
  let selectionLocation: Int?
  let selectionLength: Int?
}

private struct Event: Encodable {
  let uptimeNs: UInt64
  let utf8Bytes: Int
}

private struct Snapshot: Encodable {
  let pid: Int32
  let trial: UInt64
  let uptimeNs: UInt64
  let text: String
  let active: Bool
  let focusLost: Bool
  let events: [Event]
}

@MainActor
private final class Target: NSObject, NSApplicationDelegate, NSWindowDelegate, NSTextViewDelegate {
  private let window = NSWindow(
    contentRect: NSRect(x: 180, y: 180, width: 760, height: 260),
    styleMask: [.titled, .closable], backing: .buffered, defer: false)
  private let text = NSTextView(frame: NSRect(x: 0, y: 0, width: 760, height: 260))
  private var trial: UInt64 = 0
  private var armed = false
  private var focusLost = false
  private var events: [Event] = []

  private var active: Bool {
    NSApp.isActive && window.isKeyWindow && window.firstResponder === text
  }

  func applicationDidFinishLaunching(_ notification: Notification) {
    window.title = "Aria streaming benchmark — test text only"
    window.delegate = self
    text.autoresizingMask = [.width, .height]
    text.font = .monospacedSystemFont(ofSize: 16, weight: .regular)
    text.isAutomaticQuoteSubstitutionEnabled = false
    text.isAutomaticDashSubstitutionEnabled = false
    text.isAutomaticSpellingCorrectionEnabled = false
    text.delegate = self
    window.contentView = text
    window.makeKeyAndOrderFront(nil)
    window.makeFirstResponder(text)
    sendSnapshot()
    Task.detached { await self.readCommands() }
  }

  nonisolated private func readCommands() async {
    let maxFrameBytes = 512 * 1024
    var pending = Data()
    var buffer = [UInt8](repeating: 0, count: 4096)
    while true {
      // FileHandle.read(upToCount:) waits for a full buffer on the control socket.
      let count = Darwin.read(STDIN_FILENO, &buffer, 4096)
      if count < 0 {
        if errno == EINTR { continue }
        await fail("request read failed")
      }
      if count == 0 { break }
      pending.append(contentsOf: buffer.prefix(count))
      while let newline = pending.firstIndex(of: 10) {
        let frame = pending.prefix(upTo: newline)
        guard frame.count <= maxFrameBytes else { await fail("request too large") }
        let command: Command
        do {
          let decoder = JSONDecoder()
          decoder.keyDecodingStrategy = .convertFromSnakeCase
          command = try decoder.decode(Command.self, from: frame)
        } catch {
          await fail("invalid request")
        }
        pending.removeSubrange(...newline)
        await perform(command)
      }
      guard pending.count <= maxFrameBytes else { await fail("request too large") }
    }
    guard pending.isEmpty else { await fail("incomplete request") }
    await MainActor.run { NSApp.terminate(nil) }
  }

  private func perform(_ command: Command) async {
    switch command.op {
    case "reset":
      guard let nextTrial = command.trial, nextTrial > 0,
        let value = command.text, value.utf8.count <= 64 * 1024,
        let location = command.selectionLocation, location >= 0,
        let length = command.selectionLength, length >= 0,
        location <= value.utf16.count, length <= value.utf16.count - location
      else { fail("invalid reset") }
      armed = false
      text.string = value
      text.setSelectedRange(NSRange(location: location, length: length))
      window.makeKeyAndOrderFront(nil)
      window.makeFirstResponder(text)
      NSApp.activate(ignoringOtherApps: true)
      let deadline = ContinuousClock.now + .seconds(5)
      while !active {
        guard ContinuousClock.now < deadline else { fail("target focus timed out") }
        do {
          try await Task.sleep(for: .milliseconds(10))
        } catch {
          fail("target focus interrupted")
        }
      }
      trial = nextTrial
      events.removeAll(keepingCapacity: true)
      focusLost = false
      armed = true
      sendSnapshot()
    case "snapshot":
      guard command.trial == trial else { fail("trial mismatch") }
      sendSnapshot()
    case "quit":
      reply(["ok": true])
      NSApp.terminate(nil)
    default:
      fail("unknown operation")
    }
  }

  func textDidChange(_ notification: Notification) {
    let received = clock_gettime_nsec_np(CLOCK_UPTIME_RAW)
    guard armed else { return }
    let byteCount = text.string.utf8.count
    guard byteCount <= 64 * 1024 else { fail("target text too large") }
    guard events.count < 4096 else { fail("too many target events") }
    events.append(Event(uptimeNs: received, utf8Bytes: byteCount))
  }

  func applicationWillResignActive(_ notification: Notification) {
    if armed { focusLost = true }
  }

  func windowDidResignKey(_ notification: Notification) {
    if armed { focusLost = true }
  }

  private func sendSnapshot() {
    guard text.string.utf8.count <= 64 * 1024 else { fail("target text too large") }
    reply(
      Snapshot(
        pid: getpid(), trial: trial, uptimeNs: clock_gettime_nsec_np(CLOCK_UPTIME_RAW),
        text: text.string, active: active, focusLost: focusLost, events: events))
  }

  private func reply(_ value: some Encodable) {
    do {
      let encoder = JSONEncoder()
      encoder.keyEncodingStrategy = .convertToSnakeCase
      var frame = try encoder.encode(value)
      frame.append(10)
      try FileHandle.standardOutput.write(contentsOf: frame)
    } catch {
      let message = "benchmark target response failed\n"
      message.withCString { pointer in
        _ = Darwin.write(STDERR_FILENO, pointer, message.utf8.count)
      }
      exit(1)
    }
  }

  private func fail(_ message: String) -> Never {
    reply(["error": message])
    exit(1)
  }

  func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

signal(SIGPIPE, SIG_IGN)
let app = NSApplication.shared
app.setActivationPolicy(.regular)
private let delegate = Target()
app.delegate = delegate
app.run()
