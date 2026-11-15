%% Prices a shopping cart.
-module(cart).
-export([subtotal/1, total/1, apply_tax/1, checkout/1]).

%% The prices of the items a customer picked.
-record(cart, {items = [] :: [integer()]}).

%% Adds up the item prices.
subtotal(#cart{items = Items}) ->
    lists:sum(Items).

%% The subtotal with tax.
total(Cart) ->
    apply_tax(subtotal(Cart)).

%% Adds the sales tax to an amount.
apply_tax(Amount) ->
    Amount * 108 div 100. %% NOTE: integer division keeps whole cents

%% Prices the cart and formats the result.
checkout(Cart) ->
    Total = total(Cart),
    money:format_money(apply_tax(Total)).
