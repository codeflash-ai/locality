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
