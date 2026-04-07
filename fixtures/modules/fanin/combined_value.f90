module combined_value
    use base_value, only: base
    use offset_value, only: delta
    implicit none

    integer, parameter :: total = base + delta
end module combined_value
