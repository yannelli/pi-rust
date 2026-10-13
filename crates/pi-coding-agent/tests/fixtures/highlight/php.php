<?php
namespace App;

use App\Models\User;

class Greeter extends Base
{
    private int $count = 3;

    public function greet(string $name): string
    {
        // Greet someone
        $message = "Hello {$name}!";
        return $this->count > 0 ? $message : 'none';
    }
}

$g = new Greeter();
echo $g->greet("world"), true, null;
