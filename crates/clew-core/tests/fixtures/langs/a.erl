%% @doc A shop.
-module(shop).
-export([price/1, total/2]).
-include("shop.hrl").
-include_lib("kernel/include/file.hrl").
-record(item, {name, cost = 0}).
-type money() :: integer().
-define(TAX, 2).
-ifdef(TEST).
-endif.

%% @doc The price.
-spec price(atom()) -> money().
price(apple) -> 1;
price(_) -> 2.

total(A, B) -> lists:sum([A, B]) + other:f(1).
