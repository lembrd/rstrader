# Trade api

## Strategy API
api design for strategy implementation class:
```
base api class should have basic access to components:

md - market data component (subscribe, market_info, etc...)
oms - order managements system (start trading account, receive order, trade updates, positions handling)


```
```pseudo python code
class Strategy():
  def __init__(self, api, params)
    self.api = api
    self.oms = self.api.oms # order management system
    self.params = params


  def start(self):
    self.api.oms.start_account(self.params.account, on_account_update)
    self.api.md.subscribe(XMarketId.make(ExchangeId.BINANCE_FUTURES, 'BTCUSDT', StreamType.L2), on_l2)

  def on_l2(self, msg):
     ...

  def on_account_update(msg):
     ...
    

```