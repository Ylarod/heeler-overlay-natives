import CEasyTier
import CTailscale
import CZeroTier

/// Thin wrappers over entry points of each binary target that run without a
/// network or a started node.
public enum LinkProbe {
    /// Turns Tailscale log upload off inside the Go runtime and reports
    /// heeler_tailscale_log_upload_state (3: both switches are off).
    public static func tailscaleLogUploadState() -> Int32 {
        heeler_tailscale_disable_log_upload()
        return heeler_tailscale_log_upload_state()
    }

    /// Creates and closes a tsnet server handle without starting it.
    public static func tailscaleNewAndClose() -> (handle: Int32, close: Int32) {
        let handle = tailscale_new()
        return (handle, tailscale_close(handle))
    }

    /// Generates a ZeroTier identity and checks it with libzt.
    public static func zeroTierIdentity() -> (created: Int32, valid: Int32, length: UInt32) {
        var buffer = [CChar](repeating: 0, count: Int(ZTS_ID_STR_BUF_LEN))
        var length = UInt32(ZTS_ID_STR_BUF_LEN)
        let created = zts_id_new(&buffer, &length)
        // libzt validates the whole ZTS_ID_STR_BUF_LEN buffer, as Heeler does.
        let valid = zts_id_pair_is_valid(buffer, UInt32(buffer.count))
        return (created, valid, length)
    }

    /// heeler_zt_planet_inspect on bytes that are not a ZeroTier World.
    public static func zeroTierInspectGarbage() -> Int32 {
        let bytes: [UInt8] = [0x01, 0x02, 0x03, 0x04]
        var info = heeler_zt_planet_info()
        return bytes.withUnsafeBytes { raw in
            heeler_zt_planet_inspect(raw.baseAddress, UInt32(raw.count), &info)
        }
    }

    /// heeler_zt_peers and heeler_zt_add_moon without a running node.
    public static func zeroTierWithoutNode() -> (peers: Int32, addMoon: Int32) {
        let peers = heeler_zt_peers(nil, 0)
        let addMoon = heeler_zt_add_moon(nil, 0, 1)
        return (peers, addMoon)
    }

    /// heeler_zt_bind_network and heeler_zt_network_reaches without a
    /// running node, and their argument checks.
    public static func zeroTierNetworkBindingWithoutNode()
        -> (bind: Int32, reaches: Int32, badFamily: Int32, noAddress: Int32)
    {
        let address: [UInt8] = [10, 0, 0, 1]
        let bind = heeler_zt_bind_network(3, 0x8056_c2e2_1c00_0001, Int32(ZTS_AF_INET))
        let reaches = address.withUnsafeBytes { raw in
            heeler_zt_network_reaches(0x8056_c2e2_1c00_0001, Int32(ZTS_AF_INET), raw.baseAddress)
        }
        let badFamily = heeler_zt_bind_network(3, 0x8056_c2e2_1c00_0001, 12345)
        let noAddress = heeler_zt_network_reaches(0x8056_c2e2_1c00_0001, Int32(ZTS_AF_INET), nil)
        return (bind, reaches, badFamily, noAddress)
    }

    /// The instance key every EasyTier call takes.
    static let key = "link-probe"

    /// heeler_et_status_json for a key with no network running.
    public static func easyTierStatus() -> String {
        var buffer = [CChar](repeating: 0, count: 4096)
        let length = heeler_et_status_json(key, &buffer, buffer.count)
        guard length >= 0, length < buffer.count else { return "error \(length)" }
        return String(decoding: buffer.prefix(Int(length)).map { UInt8(bitPattern: $0) }, as: UTF8.self)
    }

    /// heeler_et_status_json with an empty (invalid) key.
    public static func easyTierStatusInvalidKey() -> Int32 {
        var buffer = [CChar](repeating: 0, count: 256)
        return heeler_et_status_json("", &buffer, buffer.count)
    }

    /// heeler_et_start with a configuration that does not parse.
    public static func easyTierStartInvalid() -> (code: Int32, message: String) {
        var error = [CChar](repeating: 0, count: 512)
        let code = heeler_et_start(key, "this is not toml = = =", 1000, &error, error.count)
        return (code, String(decoding: error.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self))
    }

    /// heeler_et_web_start (seven parameters) with a URL it must refuse,
    /// then the stop calls, and a dial on a key with nothing running.
    public static func easyTierWebStartInvalid() -> (web: Int32, dial: Int32) {
        var error = [CChar](repeating: 0, count: 512)
        let code = heeler_et_web_start(key, "not a url", "not-a-uuid", "probe", 1, &error, error.count)
        heeler_et_web_stop(key)
        heeler_et_stop(key)
        let dial = heeler_et_tcp_connect_fd(key, nil, "10.0.0.1", 22, 100, &error, error.count)
        heeler_et_stop_all()
        return (code, dial)
    }
}
