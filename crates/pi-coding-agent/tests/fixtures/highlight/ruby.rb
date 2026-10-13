require "json"

# A greeter
class Greeter < Base
  attr_reader :name

  def initialize(name = "world")
    @name = name
    @count = 42
  end

  def greet
    puts "Hello #{@name}!" if @count > 0
    [1, 2.5, :sym, nil, true].each { |x| p x }
  end
end
