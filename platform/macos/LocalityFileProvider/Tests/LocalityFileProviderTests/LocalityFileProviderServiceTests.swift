// SPDX-License-Identifier: Apache-2.0

import FileProvider
@testable import LocalityFileProvider
import Testing

@Test func extensionAdvertisesLocalityServiceSource() {
  let provider = providerExtension()
  var sources: [NSFileProviderServiceSource]?
  var serviceError: Error?

  let progress = provider.supportedServiceSources(for: .rootContainer) { resolvedSources, error in
    sources = resolvedSources
    serviceError = error
  }

  #expect(progress.completedUnitCount == 1)
  #expect(serviceError == nil)
  #expect(sources?.count == 1)
  #expect(
    sources?.first?.serviceName
      == NSFileProviderServiceName("ai.codeflash.locality.Locality.FileProvider.service")
  )
}

@Test func extensionServiceSourceVendsListenerEndpoint() throws {
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
