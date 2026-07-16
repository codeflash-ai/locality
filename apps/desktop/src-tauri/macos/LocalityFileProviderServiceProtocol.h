// SPDX-License-Identifier: Apache-2.0

#import <Foundation/Foundation.h>

@class NSXPCConnection;

NS_ASSUME_NONNULL_BEGIN

@protocol LocalityFileProviderServiceProtocol <NSObject>

- (void)fileProviderDomainIdentifierWithCompletionHandler:(void (^)(NSString *domainIdentifier))completionHandler;

@end

Protocol *LocalityFileProviderServiceProtocolForXPC(void);

typedef void (*LocalityFileProviderWarmUpCallback)(const char *_Nullable domainIdentifier, const char *_Nullable errorMessage, void *context);

void LocalityFileProviderWarmUpRemoteObject(NSXPCConnection *connection, LocalityFileProviderWarmUpCallback callback, void *context);

NS_ASSUME_NONNULL_END
