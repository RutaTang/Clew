package com.example
package shop

import scala.util.{Try, Success => Ok}
import com.example.shop.model._
import com.example.shop.repo.OrderRepository

/** An order's total.
  *
  * {{{
  * Order(1, 10.0)
  * }}}
  * See [[com.example.shop.Checkout]].
  */
case class Order(id: Long, total: Double)

trait Pricing {
  def price(order: Order): Double
}

enum Status:
  case New, Paid

object Registry {
  val names = scala.collection.mutable.ListBuffer[String]()
}

class Checkout(repo: OrderRepository) extends Pricing {
  private val rate = 0.2
  def price(order: Order): Double = {
    val base = order.total
    math.max(base * (1 + rate), 0.0)
  }
}

type Money = Double

@main def run(): Unit =
  val c = new Checkout(new OrderRepository())
  println(s"total ${c.price(Order(1, 10.0))}")
