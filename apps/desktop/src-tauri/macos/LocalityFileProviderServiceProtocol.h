// SPDX-License-Identifier: Apache-2.0

#import <Foundation/Foundation.h>

NS_ASSUME_NONNULL_BEGIN

@protocol LocalityFileProviderServiceProtocol <NSObject>

- (void)fileProviderDomainIdentifierWithCompletionHandler:(void (^)(NSString *domainIdentifier))completionHandler;

@end

Protocol *LocalityFileProviderServiceProtocolForXPC(void);

NS_ASSUME_NONNULL_END
