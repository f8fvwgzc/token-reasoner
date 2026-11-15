#import <Foundation/Foundation.h>

// Renders cents as dollars.
NSString *FormatMoney(NSInteger cents) {
    return [NSString stringWithFormat:@"$%ld.%02ld", (long)(cents / 100), (long)(cents % 100)];
}
