# A fixture for `kiln import`. It uses only providers that plan and apply
# without credentials, and exercises the constructs the importer handles:
# forward references, locals, for expressions, splats, heredocs, count,
# for_each, lifecycle and depends_on.

terraform {
  required_version = ">= 1.6"
  required_providers {
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
    local = {
      source  = "hashicorp/local"
      version = "~> 2.5"
    }
  }
}

locals {
  name      = "${var.prefix}-${random_pet.base.id}"
  tags      = { owner = "platform", "app.kubernetes.io/name" = var.prefix }
  size_list = [for k, v in var.sizes : "${k}=${v}"]
  total     = var.sizes["small"] + var.sizes.large * 2
  upper     = upper(var.prefix)
}

resource "random_pet" "base" {
  length    = 2
  separator = "-"
  prefix    = var.prefix
  keepers = {
    total = local.total
  }
}

resource "random_id" "per_size" {
  for_each    = var.sizes
  byte_length = each.value
  prefix      = "${each.key}-"
}

resource "random_integer" "extra" {
  count = var.enable_extra ? 1 : 0
  min   = 1
  max   = local.total > 5 ? 100 : 10

  lifecycle {
    ignore_changes        = [keepers]
    create_before_destroy = true
  }
}

resource "local_file" "summary" {
  filename        = "${path.module}/summary.txt"
  file_permission = "0644"
  content         = <<-EOT
    name:  ${local.name}
    sizes: ${join(", ", local.size_list)}
    ids:   ${join(",", [for r in random_id.per_size : r.hex])}
    cost:  $${not_interpolated}
  EOT

  depends_on = [random_integer.extra]
}

data "local_file" "readback" {
  filename = local_file.summary.filename
}

output "name" {
  value = local.name
}

output "first_extra" {
  value = one(random_integer.extra[*].result)
}

output "small_id" {
  value = random_id.per_size["small"].hex
}

output "tags" {
  value = merge(local.tags, { upper = local.upper })
}

output "readback_length" {
  value       = length(data.local_file.readback.content)
  description = "bytes in the summary file"
}
