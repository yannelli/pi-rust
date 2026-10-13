using System;
using System.Collections.Generic;

namespace Demo
{
    /// <summary>A greeter.</summary>
    public class Greeter : IGreeter
    {
        private readonly int _count = 3;

        [Obsolete("old")]
        public string Greet(string name)
        {
            // Build the message
            var list = new List<string> { "a", "b" };
            return $"Hello {name} {_count}" + null + true;
        }
    }
}
