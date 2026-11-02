<?php
namespace App\Models;

use App\Contracts\Priced;

/**
 * A shop item.
 */
class Item implements Priced
{
    private int $cost;

    /** The price, with {@link Tax}. */
    public function price(): int
    {
        return $this->cost + helper();
    }

    abstract protected function name(): string;
}

interface Priced { public function price(): int; }
trait HasName {}
enum Kind { case A; }

function helper(): int { return strlen("one"); }
