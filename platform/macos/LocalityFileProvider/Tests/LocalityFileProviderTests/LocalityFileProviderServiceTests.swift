// SPDX-License-Identifier: Apache-2.0

import FileProvider
@testable import LocalityFileProvider
import XCTest

final class LocalityFileProviderServiceTests: XCTestCase {
  func testExtensionAdvertisesLocalityServiceSource() {
    let provider = providerExtension()
    let expectation = expectation(description: "service sources")
    var sources: [NSFileProviderServiceSource]?
    var serviceError: Error?

    let progress = provider.supportedServiceSources(for: .rootContainer) { resolvedSources, error in
      sources = resolvedSources
      serviceError = error
      expectation.fulfill()
    }

    wait(for: [expectation], timeout: 1)
    XCTAssertEqual(progress.completedUnitCount, 1)
    XCTAssertNil(serviceError)
    XCTAssertEqual(sources?.count, 1)
    XCTAssertEqual(
      sources?.first?.serviceName,
      NSFileProviderServiceName("ai.codeflash.locality.Locality.FileProvider.service")
    )
  }

  func testExtensionServiceSourceVendsListenerEndpoint() throws {
    let provider = providerExtension()

    _ = try provider.makeListenerEndpoint()
  }

  private func providerExtension() -> LocalityFileProviderExtension {
    LocalityFileProviderExtension(
      domain: NSFileProviderDomain(
        identifier: NSFileProviderDomainIdentifier("loc"),
        displayName: "Locality"
      )
    )
  }
}
