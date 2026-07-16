// SPDX-License-Identifier: Apache-2.0

#import "LocalityFileProviderServiceProtocol.h"

Protocol *LocalityFileProviderServiceProtocolForXPC(void)
{
    return @protocol(LocalityFileProviderServiceProtocol);
}

static const char *LocalityFileProviderUTF8String(NSString *value)
{
    if (value == nil) {
        return NULL;
    }
    return [value UTF8String];
}

void LocalityFileProviderWarmUpRemoteObject(NSXPCConnection *connection, LocalityFileProviderWarmUpCallback callback, void *context)
{
    __block BOOL delivered = NO;
    void (^deliver)(NSString *, NSString *) = ^(NSString *domainIdentifier, NSString *errorMessage) {
        if (delivered) {
            return;
        }
        delivered = YES;
        [connection invalidate];
        callback(LocalityFileProviderUTF8String(domainIdentifier), LocalityFileProviderUTF8String(errorMessage), context);
    };

    @try {
        id remoteProxy = [connection remoteObjectProxyWithErrorHandler:^(NSError *error) {
            deliver(nil, error.localizedDescription ?: error.description ?: @"Could not open File Provider service connection.");
        }];
        if (![remoteProxy conformsToProtocol:@protocol(LocalityFileProviderServiceProtocol)]) {
            deliver(nil, @"File Provider service does not conform to LocalityFileProviderServiceProtocol.");
            return;
        }

        [(id<LocalityFileProviderServiceProtocol>)remoteProxy fileProviderDomainIdentifierWithCompletionHandler:^(NSString *domainIdentifier) {
            deliver(domainIdentifier, nil);
        }];
    } @catch (NSException *exception) {
        deliver(nil, exception.reason ?: exception.name ?: @"Could not call File Provider service.");
    }
}
