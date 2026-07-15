// SPDX-License-Identifier: Apache-2.0

@preconcurrency import FileProvider
@preconcurrency import Foundation

private let localityFileProviderServiceName = NSFileProviderServiceName(
  "ai.codeflash.locality.Locality.FileProvider.service"
)

@objc(LocalityFileProviderServiceProtocol)
protocol LocalityFileProviderServiceProtocol {
  func fileProviderDomainIdentifier(completionHandler: @escaping (String) -> Void)
}

extension LocalityFileProviderExtension: NSFileProviderServicing {
  func supportedServiceSources(
    for itemIdentifier: NSFileProviderItemIdentifier,
    completionHandler: @escaping ([NSFileProviderServiceSource]?, Error?) -> Void
  ) -> Progress {
    let progress = Progress(totalUnitCount: 1)
    completionHandler([self], nil)
    progress.completedUnitCount = 1
    return progress
  }
}

extension LocalityFileProviderExtension: NSFileProviderServiceSource {
  var serviceName: NSFileProviderServiceName {
    localityFileProviderServiceName
  }

  func makeListenerEndpoint() throws -> NSXPCListenerEndpoint {
    try fileProviderServiceListenerEndpoint()
  }
}

extension LocalityFileProviderExtension: NSXPCListenerDelegate {
  func listener(
    _ listener: NSXPCListener,
    shouldAcceptNewConnection newConnection: NSXPCConnection
  ) -> Bool {
    newConnection.exportedInterface = NSXPCInterface(
      with: LocalityFileProviderServiceProtocol.self
    )
    newConnection.exportedObject = self
    newConnection.resume()
    return true
  }
}

extension LocalityFileProviderExtension: LocalityFileProviderServiceProtocol {
  func fileProviderDomainIdentifier(completionHandler: @escaping (String) -> Void) {
    completionHandler(domainIdentifierForService)
  }
}
