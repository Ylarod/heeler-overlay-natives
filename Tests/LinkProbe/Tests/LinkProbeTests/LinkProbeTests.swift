import LinkProbe
import Testing

@Suite("Overlay native link probe")
struct LinkProbeTests {
    @Test func tailscaleLogUploadIsOff() {
        #expect(LinkProbe.tailscaleLogUploadState() == 3)
    }

    @Test func tailscaleHandleOpensAndCloses() {
        let result = LinkProbe.tailscaleNewAndClose()
        #expect(result.handle > 0)
        #expect(result.close == 0)
    }

    @Test func zeroTierGeneratesAValidIdentity() {
        let result = LinkProbe.zeroTierIdentity()
        #expect(result.created == 0)
        #expect(result.valid == 1)
        #expect(result.length > 0)
    }

    @Test func zeroTierRefusesAMalformedPlanet() {
        #expect(LinkProbe.zeroTierInspectGarbage() == -100)  // HEELER_ZT_PLANET_INVALID
    }

    @Test func zeroTierNeedsARunningNode() {
        let result = LinkProbe.zeroTierWithoutNode()
        #expect(result.peers < 0)
        #expect(result.addMoon < 0)
    }

    @Test func zeroTierNetworkBindingNeedsARunningNode() {
        let result = LinkProbe.zeroTierNetworkBindingWithoutNode()
        #expect(result.bind == -2)  // ZTS_ERR_SERVICE
        #expect(result.reaches == -2)
        #expect(result.badFamily == -3)  // ZTS_ERR_ARG
        #expect(result.noAddress == -3)
    }

    @Test func easyTierReportsNoNetwork() {
        #expect(LinkProbe.easyTierStatus().contains("\"running\":false"))
        #expect(LinkProbe.easyTierStatusInvalidKey() < 0)
    }

    @Test func easyTierRefusesInvalidConfigurations() {
        let start = LinkProbe.easyTierStartInvalid()
        #expect(start.code < 0)
        #expect(!start.message.isEmpty)
        let web = LinkProbe.easyTierWebStartInvalid()
        #expect(web.web < 0)
        #expect(web.dial == -3)  // HEELER_ET_ERR_NOT_RUNNING
    }
}
