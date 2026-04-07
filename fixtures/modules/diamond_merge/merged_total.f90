module merged_total
    use left_branch, only: left
    use right_branch, only: right
    implicit none

    integer, parameter :: total = left + right
end module merged_total
