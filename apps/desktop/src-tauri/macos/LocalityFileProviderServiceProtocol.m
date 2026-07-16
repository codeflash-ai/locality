// SPDX-License-Identifier: Apache-2.0

#import "LocalityFileProviderServiceProtocol.h"

#import <dispatch/dispatch.h>

static const int64_t LocalityFileProviderWarmUpConnectionHoldSeconds = 60;

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

static NSMutableArray<NSXPCConnection *> *LocalityFileProviderWarmUpConnections(void)
{
    static NSMutableArray<NSXPCConnection *> *connections = nil;
    static dispatch_once_t onceToken;
    dispatch_once(&onceToken, ^{
        connections = [NSMutableArray new];
    });
    return connections;
}

static void LocalityFileProviderHoldWarmUpConnection(NSXPCConnection *connection)
{
    NSMutableArray<NSXPCConnection *> *connections = LocalityFileProviderWarmUpConnections();
    @synchronized (connections) {
        [connections addObject:connection];
    }

    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, LocalityFileProviderWarmUpConnectionHoldSeconds * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
        [connection invalidate];
        @synchronized (connections) {
            [connections removeObjectIdenticalTo:connection];
        }
    });
}

void LocalityFileProviderWarmUpRemoteObject(NSXPCConnection *connection, LocalityFileProviderWarmUpCallback callback, void *context)
{
    LocalityFileProviderHoldWarmUpConnection(connection);

    __block BOOL delivered = NO;
    void (^deliver)(NSString *, NSString *) = ^(NSString *domainIdentifier, NSString *errorMessage) {
        if (delivered) {
            return;
        }
        delivered = YES;
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
