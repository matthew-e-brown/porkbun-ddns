# Porkbun Dynamic DNS Client

A small application, meant to be run as a service, that uses Porkbun's API to
update DNS `A` and `AAAA` records.

Some parts of this software are more complex than they need to be. That's
because I love writing code and I wanted to have fun!

## Configuration

Configuration is given in the TOML format. An example file is given below, with
all options documented.

```toml
# IPv4 (A records) and IPv6 (AAAA records) can be enabled or disabled separately.
#
# Possible options are: "enabled", "on", or 'true' to enable; "disabled", "off",
# or 'false' to disable; or a third option, "try".
#
# When set to "try", failing to determine an address does not count as an error.
# This can be useful if, for example, you would *like* to set an IPv6 address,
# but your ISP does not provide IPv6 addresses yet.
#
# If one mode is set to "try" and the other is disabled, "try" behaves the same
# as "enabled." An error will occur if that address type cannot be determined.
#
# The default is "enabled" for IPv4 and "disabled" for IPv6.
ipv4 = "enabled"
ipv6 = "try"

# A list of domains/subdomains to update the records for.
targets = [
  # For simple cases, domains may be targeted by name:
  "example.com",

  # To target a subdomain, use a table:
  { domain = "example.com", subdomain = "foo" },

  # The default TTL is 600, but it can be overridden:
  { domain = "example.com", ttl = 3600 },

  # When specifying subdomains, "@" or "" may be used to refer to the root domain:
  { domain = "example.com", subdomain = "@", ttl = 1200 },

  # Otherwise, everything is passed through as-is. These will create DNS records
  # for "*.example.com" and "*.subdomain.example.com":
  { domain = "example.com", subdomain = "*" },
  { domain = "example.com", subdomain = "*.subdomain" },
]
```

The `domain` value should match the domain name as it appears in Porkbun's
dashboard. Specifying targets with subdomains must be done using the table
syntax, since there is no (simple) way to determine where to split the main
domain and subdomain in the general case (e.g., consider
`sub2.sub1.example.co.uk`).

<!-- ----------------------------------------------------------------------- -->

## Todo

- Write systemd `.timer` and `.service` units
