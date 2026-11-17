variable "name" {
  description = "Bucket name, globally unique."
  type        = string
}

variable "location" {
  type    = string
  default = "EU"
}

variable "labels" {
  type    = map(string)
  default = {}
}

variable "versioning" {
  type = object({
    enabled     = bool
    keep_last   = optional(number, 3)
  })
  default = { enabled = false }
}

variable "kms_key" {
  description = "CMEK key for default encryption."
  type        = string
  default     = null
  sensitive   = true
}
