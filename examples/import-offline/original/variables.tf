variable "prefix" {
  type    = string
  default = "kiln"
}

variable "sizes" {
  type = map(number)
  default = {
    small = 2
    large = 4
  }
}

variable "enable_extra" {
  type    = bool
  default = true
}
