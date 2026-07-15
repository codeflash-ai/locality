// SPDX-License-Identifier: Apache-2.0

import Foundation
import Testing
@testable import LocalityFileProviderCtl

@Test func unavailableURLDoesNotReplaceAnExistingDomain() {
  #expect(!domainNeedsReplacement(.unavailable, expectedDirectoryName: "Locality"))
}

@Test func absentURLDoesNotReplaceAnExistingDomain() {
  #expect(!domainNeedsReplacement(.available(nil), expectedDirectoryName: "Locality"))
}

@Test func matchingURLKeepsTheExistingDomain() {
  let url = URL(fileURLWithPath: "/Users/test/Library/CloudStorage/Locality")
  #expect(!domainNeedsReplacement(.available(url), expectedDirectoryName: "Locality"))
}

@Test func mismatchedURLReplacesTheExistingDomain() {
  let url = URL(fileURLWithPath: "/Users/test/Library/CloudStorage/Locality-Old")
  #expect(domainNeedsReplacement(.available(url), expectedDirectoryName: "Locality"))
}

@Test func displayNameMismatchDoesNotReplaceWhenVisibleURLMatchesExpectedRoot() {
  let url = URL(fileURLWithPath: "/Users/test/Library/CloudStorage/Locality")

  #expect(
    !existingDomainNeedsReplacement(
      displayName: "Old Locality",
      requestedDisplayName: "Locality",
      visibleURLState: .available(url)
    ))
}
