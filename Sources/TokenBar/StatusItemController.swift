import AppKit
import SwiftUI
import TokenBarCore

/// Owns the NSStatusItem and its NSPopover (AppKit-hosted icon, SwiftUI
/// popover content — the codexbar pattern). Later phases talk to it through
/// `updateTitle(_:)` and `showPopover()`.
@MainActor
final class StatusItemController: NSObject {
    private let statusItem: NSStatusItem
    private let popover: NSPopover
    /// Source of truth for the popover's (user-adjustable) height.
    let chrome = PopoverChrome()
    private var defaultsObserver: NSObjectProtocol?
    private var closeObserver: NSObjectProtocol?
    private var host: NSHostingController<AnyView>?

    override init() {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        popover = NSPopover()
        super.init()

        popover.behavior = .transient
        let host = NSHostingController(rootView: AnyView(PopoverView().environmentObject(chrome)))
        self.host = host
        // The SwiftUI root has a fixed frame; let the popover keep our size
        // instead of chasing intrinsic-size updates. The real size is set per
        // open in showPopover() against the status item's actual screen.
        host.sizingOptions = []
        popover.contentSize = NSSize(width: chrome.width, height: chrome.minHeight)
        popover.contentViewController = host

        // Swap the live UI for a placeholder when the popover closes for any
        // reason (transient outside-click, Esc, programmatic close) — mirrors
        // the SettingsWindowController pattern. Without this, PopoverView's
        // .task loops run for the process lifetime because the hosting
        // controller persists across transient open/close cycles.
        closeObserver = NotificationCenter.default.addObserver(
            forName: NSPopover.didCloseNotification, object: popover, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                // didCloseNotification fires on a later runloop turn (after the
                // ~200ms close animation), by which point a fast close→reopen
                // may already have reinstalled the live view. Re-check on the
                // next turn and only blank if the popover is still closed, so a
                // reopen that landed meanwhile isn't swapped to the placeholder.
                DispatchQueue.main.async { [weak self] in
                    guard let self, !self.popover.isShown else { return }
                    self.host?.rootView = AnyView(
                        Color.clear.frame(width: self.chrome.width, height: self.chrome.minHeight))
                }
            }
        }

        // The chrome model drives the popover window from three inputs: the
        // bottom drag handle, the settings slider, and the screen-size resolve.
        chrome.onResize = { [weak popover] height, live in
            guard let popover else { return }
            popover.animates = !live // 1:1 tracking mid-drag; animate otherwise
            popover.contentSize = NSSize(width: PopoverChrome.width, height: height)
        }
        // The settings window's slider writes the height default from another
        // window — mirror it onto a live popover.
        defaultsObserver = NotificationCenter.default.addObserver(
            forName: UserDefaults.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.chrome.reloadFromDefaults() }
        }

        if let button = statusItem.button {
            button.image = NSImage(
                systemSymbolName: "chart.bar.fill",
                accessibilityDescription: "TokenBar")
            button.image?.isTemplate = true
            button.imagePosition = .imageLeft
            button.toolTip = "TokenBar"
            button.target = self
            button.action = #selector(togglePopover(_:))
            // Right-click opens the quota-source menu (battery-icon pattern).
            button.sendAction(on: [.leftMouseUp, .rightMouseUp])
        }
    }

    /// Supplies the latest quota payload for the right-click menu
    /// (AppDelegate wires this to the tray animator's cache).
    var quotaPayloadProvider: (() -> AgentUsagePayload?)?

    /// Swaps the menu-bar icon image (an animation frame).
    func setFrame(_ image: NSImage) {
        statusItem.button?.image = image
    }

    /// Whether the menu bar around the item renders dark (picks the white
    /// frame set over the black one). Status-item buttons report *vibrant*
    /// appearances (NSAppearanceNameVibrantDark), which bestMatch against
    /// [.darkAqua, .aqua] misses — match on the name instead.
    var isDarkAppearance: Bool {
        let appearance = statusItem.button?.effectiveAppearance ?? NSApp.effectiveAppearance
        return appearance.name.rawValue.localizedCaseInsensitiveContains("dark")
    }

    private var lastTitleKey = ""

    /// Sets the text shown next to the menu-bar icon ("" = icon only). A
    /// color (the quota gauge) renders as an attributed title.
    func updateTitle(_ title: String, color: NSColor? = nil) {
        guard let button = statusItem.button else { return }
        // Leading space keeps a gap between the template icon and the text.
        let value = title.isEmpty ? "" : " \(title)"
        let key = "\(value)|\(color?.description ?? "")"
        if key != lastTitleKey {
            lastTitleKey = key
            if let color, !value.isEmpty {
                button.attributedTitle = NSAttributedString(
                    string: value,
                    attributes: [
                        .font: NSFont.menuBarFont(ofSize: 0),
                        .foregroundColor: color,
                    ])
            } else {
                button.title = value
            }
        }
        button.imagePosition = value.isEmpty ? .imageOnly : .imageLeft
    }

    func showPopover() {
        guard !popover.isShown, let button = statusItem.button else { return }
        // Reinstall the live PopoverView so its .task loops start fresh
        // (the previous close swapped it for a static placeholder).
        host?.rootView = AnyView(PopoverView().environmentObject(chrome))
        // Size against the screen the status item actually lives on (reliable,
        // unlike NSScreen.main at launch with no key window) every time we open.
        let visible = (button.window?.screen ?? NSScreen.main)?.visibleFrame.height ?? 900
        chrome.resolve(visibleHeight: visible)
        // Accessory apps are never frontmost; activate so the transient
        // popover gets key status and closes on outside clicks.
        NSApp.activate(ignoringOtherApps: true)
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
    }

    func closePopover() {
        popover.performClose(nil)
    }

    /// Clean teardown on app termination: drop the defaults observer, close
    /// the popover, and remove the status item so ControlCenter tears the
    /// menu-bar item down via a normal removal instead of an abrupt
    /// connection-invalidation — the latter left RunningBoard "waiting on
    /// exit context" for ~40s on quit (seen in the 2026-06-16 freeze logs).
    func tearDown() {
        if let defaultsObserver { NotificationCenter.default.removeObserver(defaultsObserver) }
        defaultsObserver = nil
        if let closeObserver { NotificationCenter.default.removeObserver(closeObserver) }
        closeObserver = nil
        if popover.isShown { popover.performClose(nil) }
        popover.contentViewController = nil
        NSStatusBar.system.removeStatusItem(statusItem)
    }

    @objc private func togglePopover(_ sender: Any?) {
        if NSApp.currentEvent?.type == .rightMouseUp {
            showQuotaMenu()
            return
        }
        if popover.isShown {
            closePopover()
        } else {
            showPopover()
        }
    }

    // MARK: - Right-click quota-source menu

    /// "What does the icon track?" — Auto plus every known quota window with
    /// its live remaining percent, checkmark on the current pick. Writes the
    /// same defaults key the settings panel edits; the icon follows within a
    /// couple of seconds.
    private func showQuotaMenu() {
        let menu = NSMenu()
        let current = UserDefaults.standard.string(forKey: TrayAnimator.quotaSourceKey)
            ?? QuotaResolver.auto

        let header = NSMenuItem(title: "Menu bar tracks", action: nil, keyEquivalent: "")
        header.isEnabled = false
        menu.addItem(header)

        func add(_ title: String, selection: String) {
            let item = NSMenuItem(
                title: title, action: #selector(pickQuotaSource(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = selection
            item.state = selection == current ? .on : .off
            menu.addItem(item)
        }
        add("Auto (tightest window)", selection: QuotaResolver.auto)

        if let payload = quotaPayloadProvider?() {
            for agent in payload.agents where agent.error == nil && !agent.windows.isEmpty {
                menu.addItem(.separator())
                let name = NSMenuItem(
                    title: ClientRegistry.style(agent.clientId).displayName,
                    action: nil, keyEquivalent: "")
                name.isEnabled = false
                menu.addItem(name)
                for window in agent.windows {
                    let left = "\(Int(min(100, max(0, window.remainingPercent)).rounded()))% left"
                    add(
                        "\(window.label) — \(left)",
                        selection: QuotaResolver.selection(
                            clientId: agent.clientId, label: window.label))
                }
            }
        } else {
            let loading = NSMenuItem(title: "Loading quotas…", action: nil, keyEquivalent: "")
            loading.isEnabled = false
            menu.addItem(loading)
        }

        // Pop up via a transient menu assignment so the next left-click still
        // toggles the popover instead of re-opening the menu.
        statusItem.menu = menu
        statusItem.button?.performClick(nil)
        statusItem.menu = nil
    }

    @objc private func pickQuotaSource(_ sender: NSMenuItem) {
        guard let selection = sender.representedObject as? String else { return }
        UserDefaults.standard.set(selection, forKey: TrayAnimator.quotaSourceKey)
    }
}
