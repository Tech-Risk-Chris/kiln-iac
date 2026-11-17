resource "google_storage_bucket" "this" {
  name                        = var.name
  location                    = var.location
  labels                      = var.labels
  uniform_bucket_level_access = true

  versioning {
    enabled = var.versioning.enabled
  }

  dynamic "lifecycle_rule" {
    for_each = var.versioning.enabled ? [1] : []
    content {
      action {
        type = "Delete"
      }
      condition {
        num_newer_versions = var.versioning.keep_last
      }
    }
  }

  dynamic "encryption" {
    for_each = var.kms_key == null ? [] : [var.kms_key]
    content {
      default_kms_key_name = encryption.value
    }
  }
}
