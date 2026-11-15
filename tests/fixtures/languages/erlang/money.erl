%% Formats amounts of money.
-module(money).
-export([format_money/1]).

%% Renders cents as dollars.
format_money(Cents) ->
    io_lib:format("$~B.~2..0B", [Cents div 100, Cents rem 100]).
