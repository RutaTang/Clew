package com.example.shop

import kotlin.math.max
import com.example.shop.model.*
import com.example.shop.repo.OrderRepository as Repo

/** An order's total, with [tax] applied. */
data class Order(val id: Long, val total: Double)

interface Pricing {
    fun price(order: Order): Double
}

enum class Status { NEW, PAID }

object Registry {
    val names = mutableListOf<String>()
}

class Checkout(private val repo: Repo) : Pricing {
    private val rate = 0.2

    override fun price(order: Order): Double {
        val base = order.total
        return max(base * (1 + rate), 0.0)
    }

    companion object Factory {
        fun create(): Checkout = Checkout(Repo())
    }
}

typealias Money = Double

fun main() {
    val c = Checkout.create()
    println("total ${c.price(Order(1, 10.0))} and $c")
    val ok = true && null == null
}
