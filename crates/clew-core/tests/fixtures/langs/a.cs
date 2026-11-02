using System;

namespace Shop.Models
{
    /// <summary>An item, see <see cref="T:Shop.Kind"/>.</summary>
    public class Item : IPriced
    {
        public int Cost { get; set; }

        /// <summary>The price.</summary>
        /// <param name="tax">The tax.</param>
        /// <returns>The <c>total</c>.</returns>
        [Obsolete("Use Total")]
        public int Price(int tax) => Cost + tax;

        [Fact]
        public void PriceWorks() { }

        private void Helper() { }
    }

    public interface IPriced { int Price(int tax); }
    public enum Kind { A, B }
    public record Money(int Value);
    public struct Point { }
}
