import UIKit
import WebKit

/// Fast mode's page: a web view, where WebKit may compile r86's Wasm JIT.
/// The page runs the CPU, memory and devices and draws the screen; r86 serves
/// it from 127.0.0.1 and keeps the disks (r86_vm_start_web). Touches and keys
/// stay with GuestUIView, which shows this view under them.
@MainActor
final class WebEngine: NSObject, WKNavigationDelegate {
    let view: WKWebView
    /// The page could not be loaded.
    private(set) var failed = false
    private var loaded = false
    private var smooth = false

    /// Lockdown Mode applies to this app's web views.
    static var lockdown: Bool { WKWebpagePreferences().isLockdownModeEnabled }

    init(url: URL) {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        view = WKWebView(frame: CGRect(x: 0, y: 0, width: 1, height: 1), configuration: configuration)
        super.init()
        view.navigationDelegate = self
        view.isUserInteractionEnabled = false
        // Black, not white, until the page draws.
        view.isOpaque = false
        view.backgroundColor = .black
        view.scrollView.backgroundColor = .black
        view.scrollView.isScrollEnabled = false
        view.scrollView.bounces = false
        view.scrollView.contentInsetAdjustmentBehavior = .never
        park()
        view.load(URLRequest(url: url))
    }

    /// Linear filtering when the guest screen is scaled.
    func setSmooth(_ on: Bool) {
        guard on != smooth else { return }
        smooth = on
        applySmooth()
    }

    private func applySmooth() {
        guard loaded else { return }
        view.evaluateJavaScript("document.body.classList.toggle('smooth', \(smooth))", completionHandler: nil)
    }

    /// Off screen but still in the window: WebKit slows or suspends a page
    /// whose view has left it, and the page is the machine.
    func park() {
        let window = UIApplication.shared.connectedScenes
            .compactMap { ($0 as? UIWindowScene)?.keyWindow }.first
        view.frame = CGRect(x: 0, y: 0, width: 1, height: 1)
        if let window {
            window.insertSubview(view, at: 0)
        } else {
            view.removeFromSuperview()
        }
    }

    func close() {
        view.navigationDelegate = nil
        view.stopLoading()
        view.removeFromSuperview()
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        loaded = true
        applySmooth()
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        failed = true
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!,
                 withError error: Error) {
        failed = true
    }
}
