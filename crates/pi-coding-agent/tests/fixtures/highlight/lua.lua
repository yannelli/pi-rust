-- A greeter
local Greeter = {}
Greeter.__index = Greeter

function Greeter.new(name)
  local self = setmetatable({}, Greeter)
  self.name = name or "world"
  self.count = 42
  return self
end

function Greeter:greet(times)
  for i = 1, times do
    print(string.format("Hello %s %d", self.name, i))
  end
  return nil, true, 3.5
end

--[[ block
comment ]]
return Greeter
