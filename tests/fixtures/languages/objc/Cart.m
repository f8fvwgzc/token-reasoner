// Prices a shopping cart.
#import <Foundation/Foundation.h>

NSString *FormatMoney(NSInteger cents);

// The prices of the items a customer picked.
@interface Cart : NSObject
@property(nonatomic, copy) NSArray<NSNumber *> *items;
- (NSInteger)subtotal;
- (NSInteger)total;
@end

// Adds the sales tax to an amount.
NSInteger ApplyTax(NSInteger amount) {
    return amount * 108 / 100;
}

@implementation Cart

// Adds up the item prices.
- (NSInteger)subtotal {
    NSInteger sum = 0;
    for (NSNumber *price in self.items) {
        sum += price.integerValue;
    }
    return sum;
}

// The subtotal with tax.
- (NSInteger)total {
    return ApplyTax([self subtotal]);
}

@end

// Prices the cart and formats the result.
NSString *Checkout(Cart *cart) {
    NSInteger total = [cart total];
    return FormatMoney(ApplyTax(total));
}
