// SPDX-License-Identifier: Apache-2.0

import Foundation

enum UserVisibleDomainURLState {
  case available(URL?)
  case unavailable
}

func domainNeedsReplacement(
  _ state: UserVisibleDomainURLState,
  expectedDirectoryName: String
) -> Bool {
  switch state {
  case .unavailable, .available(nil):
    return false
  case .available(let url?):
    return url.lastPathComponent != expectedDirectoryName
  }
}

func existingDomainNeedsReplacement(
  displayName _: String,
  requestedDisplayName: String,
  visibleURLState: UserVisibleDomainURLState
) -> Bool {
  domainNeedsReplacement(
    visibleURLState,
    expectedDirectoryName: fileProviderDirectoryName(for: requestedDisplayName)
  )
}

func fileProviderDirectoryName(for displayName: String) -> String {
  if displayName.isEmpty {
    return "Locality"
  }
  if displayName == "Locality" || displayName.hasPrefix("Locality-") {
    return displayName
  }
  return "Locality-\(displayName)"
}

func fileProviderDomainIsUsable(
  userEnabled: Bool,
  disconnected: Bool,
  hidden: Bool
) -> Bool {
  userEnabled && !disconnected && !hidden
}
